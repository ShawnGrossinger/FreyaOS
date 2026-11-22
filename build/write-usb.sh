#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# write-usb.sh — write the FreyaOS image to a USB stick, carefully, and
# turn it back into an ordinary stick afterwards.
#
#   make usb DEV=/dev/sdX          # builds first, then runs this
#   ./write-usb.sh out/freya-os.img /dev/sdX
#   make usb-clear DEV=/dev/sdX    # back to an empty FAT32 stick (LABEL=... to name it)
#   ./write-usb.sh --clear /dev/sdX
#   ./write-usb.sh --list          # show the USB disks this would accept
#
# Everything on the stick is lost. To make sure that's the only thing lost,
# both modes refuse a device that:
#   - isn't a whole disk (a partition like /dev/sdb1 is refused)
#   - isn't attached over USB
#   - holds the running system's root filesystem, or anything mounted,
#     or anything in use as swap, LVM or an encrypted volume
#   - is bigger than MAX_GIB (default 256) — a USB backup drive, most likely
# then shows what's on the device and waits for its name to be typed back.
#
# After writing it reads the stick back (bypassing the page cache) and
# compares it with the image. Clearing zeroes the first MiB (FreyaOS's boot
# code and partition table), makes one FAT32 partition over the whole stick,
# and formats it, so Windows, macOS and Linux all read it.
# ---------------------------------------------------------------------------
set -euo pipefail

MAX_GIB=${MAX_GIB:-256}
LABEL=${LABEL:-USB}
PATH=$PATH:/usr/sbin:/sbin               # wipefs, blockdev, sfdisk live here on Debian/Kali

die() { echo "write-usb: $*" >&2; exit 1; }

list_usb_disks() {
  echo "USB disks (the stick is usually the one whose size and model you recognise):"
  lsblk -dpno NAME,TRAN,SIZE,VENDOR,MODEL,RM | awk '$2 == "usb" { print "  " $0 }' || true
  echo "(Plug the stick in, run this again, and compare: the new line is the stick.)"
}

[ "$(uname -s)" = Linux ] || die "this script is for Linux (it uses lsblk, findmnt and /sys)"
for tool in lsblk findmnt dd cmp wipefs blockdev sudo; do
  command -v "$tool" >/dev/null || die "needs '$tool', which isn't installed"
done

usage() {
  echo "usage: $0 <image> /dev/sdX     write the image" >&2
  echo "       $0 --clear /dev/sdX     make it an empty FAT32 stick again" >&2
  echo "       $0 --list               list USB disks" >&2
  list_usb_disks >&2
  exit 2
}

if [ "${1:-}" = --list ]; then list_usb_disks; exit 0; fi
if [ "${1:-}" = --clear ]; then
  [ $# -eq 2 ] || usage
  mode=clear img= dev=$2
  for tool in sfdisk mkfs.vfat; do
    command -v "$tool" >/dev/null || die "clearing needs '$tool': sudo apt install fdisk dosfstools"
  done
  [[ "$LABEL" =~ ^[A-Za-z0-9_-]{1,11}$ ]] || die "LABEL must be 1 to 11 letters, digits, - or _"
else
  [ $# -eq 2 ] || usage
  mode=write img=$1 dev=$2
  [ -f "$img" ] || die "no image at $img (run make first)"
fi
[ -b "$dev" ] || die "$dev isn't a block device"
dev=$(readlink -f "$dev")                 # follow /dev/disk/by-id/... links
name=${dev##*/}

# --- 1. A whole disk, on USB, not too big ---------------------------------
type=$(lsblk -dno TYPE "$dev")
[ "$type" = disk ] || die "$dev is a '$type', not a whole disk (use /dev/sdX, not /dev/sdX1)"
tran=$(lsblk -dno TRAN "$dev" | tr -d ' ')
[ "$tran" = usb ] || die "$dev is attached over '${tran:-unknown}', not USB: refusing (this is how the laptop's own drive is protected)"
bytes=$(lsblk -bdno SIZE "$dev" | tr -d ' ')
[ "$bytes" -gt 0 ] || die "$dev reports a size of 0 (no medium in a card reader?)"
[ "$bytes" -le $((MAX_GIB * 1024 * 1024 * 1024)) ] \
  || die "$dev is $((bytes / 1024 / 1024 / 1024)) GiB, over the ${MAX_GIB} GiB limit: probably not a stick (MAX_GIB=... to override)"
if [ "$mode" = write ]; then
  img_bytes=$(stat -c %s "$img")
  [ $((img_bytes % 1048576)) -eq 0 ] || die "$img isn't a whole number of MiB: rebuild it with make"
  [ "$img_bytes" -le "$bytes" ] || die "the image ($img_bytes bytes) is bigger than $dev"
else
  [ "$bytes" -ge $((64 * 1024 * 1024)) ] || die "$dev is under 64 MiB: too small for FAT32"
fi

# --- 2. Not the system disk, nothing in use --------------------------------
root_src=$(findmnt -no SOURCE / || true)
root_src=${root_src%%\[*}                 # btrfs shows /dev/sda2[/@]
root_disk=$(lsblk -no PKNAME "$root_src" 2>/dev/null | head -1 || true)
for d in $(lsblk -nro NAME "$dev"); do
  # Any partition (or the disk) of this device that the root filesystem sits on, even through LVM/crypt.
  if lsblk -nro NAME -s "$root_src" 2>/dev/null | grep -qx "$d"; then
    die "$dev holds the running system's root filesystem: refusing"
  fi
done
[ "$root_disk" != "$name" ] || die "$dev holds the running system's root filesystem: refusing"

mounted=$(lsblk -nro NAME,MOUNTPOINT "$dev" | awk 'NF >= 2 { print "    /dev/" $1 " on " $2 }')
if [ -n "$mounted" ]; then
  echo "write-usb: something on $dev is mounted:" >&2
  echo "$mounted" >&2
  echo "Unmount it first (nothing else is touched), e.g.:" >&2
  lsblk -nro NAME,MOUNTPOINT "$dev" | awk 'NF >= 2 { print "    udisksctl unmount -b /dev/" $1 }' >&2
  exit 1
fi
swaps=$(lsblk -nro NAME "$dev" | while read -r d; do grep -q "^/dev/$d " /proc/swaps && echo "/dev/$d"; done || true)
[ -z "$swaps" ] || die "$swaps is in use as swap: refusing"
for d in $(lsblk -nro NAME "$dev"); do
  holders=/sys/class/block/$d/holders
  if [ -d "$holders" ] && [ -n "$(ls -A "$holders")" ]; then
    die "/dev/$d is in use by $(ls "$holders" | tr '\n' ' ')(LVM, RAID or an encrypted volume): refusing"
  fi
done

# --- 3. Show it, and ask ----------------------------------------------------
echo
if [ "$mode" = write ]; then
  echo "About to write $img ($img_bytes bytes) to:"
else
  echo "About to clear this device and format it as one empty FAT32 partition labelled $LABEL:"
fi
echo
lsblk -o NAME,TRAN,SIZE,RM,VENDOR,MODEL,SERIAL,FSTYPE,LABEL "$dev" | sed 's/^/    /'
echo
echo "EVERYTHING ON THIS DEVICE WILL BE LOST."
echo "The running system is on: ${root_src:-unknown} (not touched)."
echo
identity=$(lsblk -bdno SIZE,MODEL,SERIAL "$dev")
read -r -p "Type the device name ($name) to continue, anything else to stop: " answer
[ "$answer" = "$name" ] || { echo "Stopped. Nothing was written."; exit 1; }
[ "$(lsblk -bdno SIZE,MODEL,SERIAL "$dev" 2>/dev/null)" = "$identity" ] \
  || die "$dev changed while waiting (unplugged? another device?): stopped, nothing written"

# --- 4a. Clear ----------------------------------------------------------------
if [ "$mode" = clear ]; then
  # 2 MiB: FreyaOS's first MiB, plus where the new partition's boot sectors
  # go, so an older filesystem left there can't reappear inside it.
  echo "Clearing old signatures and the first 2 MiB of $dev..."
  sudo wipefs --all --quiet "$dev"
  sudo dd if=/dev/zero of="$dev" bs=1M count=2 conv=fsync status=none
  echo "Making one FAT32 partition over the whole stick..."
  # MBR ("dos") table; the partition starts at 1 MiB; type c is FAT32 (LBA).
  printf 'label: dos\nstart=2048, type=c\n' | sudo sfdisk --quiet "$dev"
  sudo blockdev --rereadpt "$dev" 2>/dev/null || true
  part=
  for _ in $(seq 1 25); do                  # wait (up to 5 s) for the new partition's device
    part=$(lsblk -nrpo NAME,TYPE "$dev" | awk '$2 == "part" { print $1; exit }')
    [ -n "$part" ] && [ -b "$part" ] && break
    part=; sleep 0.2
  done
  [ -n "$part" ] || die "the new partition didn't appear: unplug the stick, plug it back in, and run this again"
  command -v udevadm >/dev/null && sudo udevadm settle 2>/dev/null || true
  # The desktop may have mounted something it found there in the meantime.
  # It's on the stick being cleared, so unmount it (mkfs would refuse anyway).
  if findmnt -rno TARGET "$part" >/dev/null 2>&1; then
    sudo umount "$part" || die "$part was mounted by the desktop and won't unmount: close any window showing it and run this again"
  fi
  sudo wipefs --all --quiet "$part"
  echo "Formatting $part as FAT32 ($LABEL)..."
  sudo mkfs.vfat -F 32 -n "$LABEL" "$part" >/dev/null
  sync
  echo
  echo "Done: $dev is an empty FAT32 stick labelled $LABEL. FreyaOS is no longer on it."
  echo "Unplug it and plug it back in to use it, or remove it safely first with:"
  if command -v udisksctl >/dev/null; then echo "    udisksctl power-off -b $dev"; else echo "    sudo eject $dev"; fi
  exit 0
fi

# --- 4. Write ---------------------------------------------------------------
# wipefs removes old partition-table signatures from the whole stick (an old
# GPT keeps a backup copy at the far end, which tools might "restore").
echo "Clearing old partition-table signatures on $dev..."
sudo wipefs --all --quiet "$dev"
echo "Writing..."
sudo dd if="$img" of="$dev" bs=1M conv=fsync status=progress
sudo blockdev --flushbufs "$dev"

# --- 5. Read it back and compare ---------------------------------------------
echo "Reading it back to check..."
if sudo dd if="$dev" bs=1M count=$((img_bytes / 1048576)) iflag=direct status=none | cmp -s - "$img"; then
  echo "Verified: the stick matches the image."
else
  die "the stick does NOT match the image: try again, or try another stick"
fi
sync

echo
echo "Done. To remove the stick safely:"
if command -v udisksctl >/dev/null; then
  echo "    udisksctl power-off -b $dev"
else
  echo "    sudo eject $dev"
fi
echo "then unplug it. (Nothing is mounted from it, so pulling it now is safe too.)"
