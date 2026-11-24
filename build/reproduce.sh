#!/bin/bash
# reproduce.sh — build FreyaOS a second time, from a copy of the tree in
# another directory, and compare the two images (Entry 30).
#
#   make reproduce        (or ./reproduce.sh)
#
# They must be the same, byte for byte: nothing in the image may depend on
# where the source is, who built it, or when. Built with the same compiler
# (../rust-toolchain.toml), another machine's image should then match this
# one's too. This checks the part one machine can check: the copy's build
# starts from nothing (no target/ directories) in a directory whose path
# shares nothing with this one.
set -u
cd "$(dirname "$0")"
tree=$(cd .. && pwd)
unset FREYA_DEMO BIOS_LEAVES_IF_ON TEXT_MODE TEST_VBE_LIES

tmp=$(mktemp -d "${TMPDIR:-/tmp}/freya-reproduce.XXXXXX")
copy="$tmp/a-copy-of-FreyaOS-somewhere-else"
mkdir -p "$copy"

echo "reproduce: building in $tree"
if ! make -s all > "$tmp/here.log" 2>&1; then
  echo "reproduce: the build here failed:"; tail -20 "$tmp/here.log"; exit 1
fi

# The tree as it is now, committed or not, without build output or git.
tar -C "$tree" --exclude=./.git --exclude=./build/out --exclude=./kernel/target \
    --exclude=./user/rust/target -cf - . | tar -C "$copy" -xf -
echo "reproduce: building a copy from nothing in $copy"
if ! (cd "$copy/build" && make -s all) > "$tmp/copy.log" 2>&1; then
  echo "reproduce: the copy's build failed:"; tail -20 "$tmp/copy.log"; exit 1
fi

a=out/freya-os.img
b="$copy/build/out/freya-os.img"
ha=$(sha256sum "$a" | cut -d' ' -f1)
hb=$(sha256sum "$b" | cut -d' ' -f1)
kernel=$(stat -c %s out/kernel.bin)
echo "reproduce: compiler $(rustc --version)"
echo "           here     sha256 $ha"
echo "           the copy sha256 $hb"
if [ "$ha" = "$hb" ]; then
  echo "reproduce: PASS: the same image, $(stat -c %s "$a") bytes (kernel $((kernel / 512)) sectors)"
  rm -rf "$tmp"
  exit 0
fi

echo "reproduce: FAIL: the images differ in $(cmp -l "$a" "$b" | wc -l) bytes"
for part in stage1.bin stage2.bin kernel.bin $(cd out && ls user/*.bin); do
  cmp -s "out/$part" "$copy/build/out/$part" || echo "           differs: $part"
done
# The usual cause: a path, which shows up as text.
if grep -q -a -F "$copy" "$b"; then
  echo "           the copy's image contains its own path ($copy)"
fi
if grep -q -a -F "$HOME" "$a"; then
  echo "           the image contains your home directory ($HOME)"
fi
echo "reproduce: the copy and its logs are kept in $tmp"
exit 1
