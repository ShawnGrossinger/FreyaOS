#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# test.sh — build and boot FreyaOS headless in every test configuration,
# check each run's serial output, and print a summary.
#
#   make test                 # all cases
#   ./test.sh nx panic        # just these cases
#   TIMEOUT=120 ./test.sh     # slower machine
#
# Each case gets its own build in out/test/<case>/ (serial.log is the
# kernel's COM1 output), so it never disturbs the normal out/ image.
#
# Every case must also show "self-tests: N passed, 0 failed" and no
# ": FAIL" line: the self-tests run before any demo fires.
# ---------------------------------------------------------------------------
set -u
cd "$(dirname "$0")"

QEMU=${QEMU:-qemu-system-x86_64}
TIMEOUT=${TIMEOUT:-60}
ROOT=out/test

# name | FREYA_DEMO (or bios-if-on: see below) | RAM | patterns that must
#        all appear, separated by ;;
#      | optional extra QEMU arguments
#      | optional keys to press once the keyboard driver is ready (QEMU
#        sendkey names, space-separated). Afterwards "QEMU exited by
#        itself" is logged if the machine restarted or switched off.
#        Some names aren't keys: "screendump" saves the screen QEMU shows
#        and logs whether its size is the mode the kernel reported, and
#        "hold-<key>" (hold-shift, hold-ctrl) holds that key down,
#        repeating like a real keyboard, from stage 2 until just after the
#        keyboard driver is ready (keys after it are pressed once it has
#        been let go).
CASES=(
  "image||||"   # not booted: checks the disk image's layout (see check_image)
  "boot-128M||128M|init complete - boot thread done; scheduler and timer running;;\\[display\\] VESA graphics [0-9]+x[0-9]+, 32-bit;;reads back pixel for pixel \\(128 pixels, [0-9]+ lit, 0 wrong\\); framebuffer mapped write-combining \\(PAT entry 4\\): PASS;;self-test [0-9]+ ticks in 100 ms;;sleep 5 ticks woke after;;\\[user [0-9]+ 'hello'\\] Hello from ring 3!;;'probe' killed by vector 14 #PF;;\\[user [0-9]+ 'ipc'\\] printed for me by the log server;;driver handled 10"
  "bios-if-on|bios-if-on|128M|BIOS had left them ON after its disk calls; stage 2 turned them off\\): PASS;;self-tests: [0-9]+ passed, 0 failed"
  "dirty-ram||128M-dirty|self-tests: [0-9]+ passed, 0 failed;;init complete"
  "boot-4G||4G|physical map at 0xffff800000000000: 4096 MiB;;init complete - boot thread done; scheduler and timer running"
  "ud2|ud2|128M|vector 6 #UD Invalid Opcode"
  "page_fault|page_fault|128M|vector 14 #PF Page Fault;;cr2=0x0000000010000000;;page not present, read"
  "stack_overflow|stack_overflow|128M|vector 8 #DF Double Fault;;KERNEL STACK OVERFLOW;;running task: 0 'boot'"
  "task_stack_overflow|task_stack_overflow|128M|vector 8 #DF Double Fault;;TASK STACK OVERFLOW \(task [0-9]+ 'overflow-demo';;running task: [0-9]+ 'overflow-demo'"
  "smap|smap|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, read, supervisor mode;;kernel touched user memory|-cpu max"
  "smep|smep|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, instruction fetch, supervisor mode;;kernel touched user memory|-cpu max"
  "keyboard||128M|keyboard ready;;typed: hello World||delete end ctrl-delete alt-end ctrl-end alt-delete h e l l o spc shift-w o r l d ret"
  "scroll||128M|scrolling checked: [0-9 ]+;;typed: hi;;screendump matches the mode: [0-9]+x[0-9]+, and the colour swatch is the VGA palette||pgdn pgup pgup pgdn pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup h i ret screendump"
  "text-mode|text-mode|128M|text mode, 80x25: built with TEXT_MODE=1;;a character put in the last cell of text mode is in VGA memory .*: PASS;;\\[console\\] view 6 rows back;;\\[console\\] view 12 rows back;;screendump matches the mode: 720x400||pgup pgup screendump"
  "edid-1366||128M|display's own resolution 1366x768 \\(EDID\\);;VESA graphics 1280x768, 32-bit;;screendump matches the mode: 1280x768, and the colour swatch is the VGA palette|-vga none -device VGA,xres=1366,yres=768|screendump"
  "edid-1440||128M|display's own resolution 1440x900 \\(EDID\\);;VESA graphics 1440x900, 32-bit|-vga none -device VGA,xres=1440,yres=900|"
  "no-edid||128M|display's own resolution unknown \\(no EDID: modes up to 1024x768\\);;VESA graphics 1024x768, 32-bit;;screendump matches the mode: 1024x768, and the colour swatch|-vga none -device VGA,edid=off|screendump"
  "cirrus-24bit||128M|VESA graphics 1024x768, 24-bit;;reads back pixel for pixel \\(128 pixels, [0-9]+ lit, 0 wrong\\);;screendump matches the mode: 1024x768, and the colour swatch|-vga cirrus|screendump"
  "shift||128M|display: status 0x02;;text mode, 80x25: Shift held while booting;;a character put in the last cell of text mode is in VGA memory .*: PASS;;typed: hi|-global i8042.kbd-throttle=on|hold-shift h i ret"
  "held-key||128M|VESA graphics [0-9]+x[0-9]+;;typed: hi|-global i8042.kbd-throttle=on|hold-ctrl h i ret"
  "restart||128M|\\[power\\] Restarting;;restart through the keyboard controller;;QEMU exited by itself||ctrl-alt-delete"
  "shutdown||128M|switching the firmware to ACPI mode;;\\[power\\] ACPI soft off \\(S5\\);;QEMU exited by itself||ctrl-alt-end"
  "restart-q35||128M|restart through the ACPI reset register;;QEMU exited by itself|-machine q35|ctrl-alt-delete"
  "nx|nx|128M|vector 14 #PF Page Fault;;protection violation, instruction fetch"
  "use_after_unmap|use_after_unmap|128M|vector 14 #PF Page Fault;;cr2=0xffffc00000100000;;page not present, read"
  "write_read_only|write_read_only|128M|vector 14 #PF Page Fault;;protection violation, write"
  "heap_double_free|heap_double_free|128M|\[heap\] FATAL: double free"
  "panic|panic|128M|\[kernel\] PANIC at src/main.rs:[0-9]+: FREYA_DEMO=panic"
)
COMMON="self-tests: [0-9]+ passed, 0 failed"

# A line on serial that means the kernel has stopped for good.
TERMINAL='init complete|\[EXCEPTION\] halted|FATAL|PANIC'

boot() { # boot <image> <ram> <serial log> <keys> [extra QEMU arguments...]
  local img=$1 mem=$2 log=$3 keys=$4
  shift 4
  local memargs=(-m "$mem")
  # Big guests: back RAM without reserving it up front (the guest only
  # touches a few MiB), so the host needn't have that much free.
  case $mem in *G) memargs=(-object "memory-backend-ram,id=ram0,size=$mem,reserve=off"
                             -machine memory-backend=ram0 -m "$mem") ;; esac
  # "<size>-dirty": RAM starts full of random bytes, as on a real machine
  # (QEMU's starts zeroed), from a file mapped privately as guest RAM.
  local dirty=
  case $mem in *-dirty)
    mem=${mem%-dirty}; dirty=${log%/*}/dirty.ram
    head -c "$mem" /dev/urandom > "$dirty" 2>/dev/null \
      || dd if=/dev/urandom of="$dirty" bs=1M count="${mem%M}" status=none
    memargs=(-object "memory-backend-file,id=ram0,size=$mem,mem-path=$dirty,share=off"
             -machine memory-backend=ram0 -m "$mem") ;;
  esac

  # Keys go in through QEMU's monitor, over a pair of named pipes.
  local mon=${log%/*}/monitor monitor=(-monitor none)
  if [ -n "$keys" ]; then
    rm -f "$mon.in" "$mon.out"; mkfifo "$mon.in" "$mon.out"
    monitor=(-monitor "pipe:$mon")
  fi
  # Holding a key down takes QMP, the machine protocol: the monitor's
  # sendkey always lets go again.
  local qmp=${log%/*}/qmp qmpargs=() hold= k
  for k in $keys; do
    case $k in hold-*) hold=${k#hold-} ;; esac
  done
  if [ -n "$hold" ]; then
    rm -f "$qmp.in" "$qmp.out"; mkfifo "$qmp.in" "$qmp.out"
    qmpargs=(-qmp "pipe:$qmp")
  fi

  : > "$log"
  "$QEMU" -drive "format=raw,file=$img,if=ide,media=disk,index=0" "${memargs[@]}" \
          -display none "${monitor[@]}" "${qmpargs[@]}" -serial "file:$log" -no-reboot \
          -d int,cpu_reset -D "${log%/*}/qemu.log" "$@" </dev/null >/dev/null 2>&1 &
  local pid=$! ticks=0
  local reader= qmp_reader= holder=
  if [ -n "$keys" ]; then cat "$mon.out" > /dev/null & reader=$!; fi   # drain the monitor's replies
  if [ -n "$hold" ]; then
    cat "$qmp.out" > /dev/null & qmp_reader=$!
    hold_key "$hold" "$qmp.in" "$log" "$pid" & holder=$!
  fi
  while kill -0 "$pid" 2>/dev/null; do
    if grep -qE "$TERMINAL" "$log"; then sleep 0.5; break; fi   # let the report finish
    sleep 0.2
    ticks=$((ticks + 1))
    if [ "$ticks" -ge $((TIMEOUT * 5)) ]; then
      echo "[test.sh] TIMEOUT: no terminal line after ${TIMEOUT}s" >> "$log"
      break
    fi
  done
  if ! kill -0 "$pid" 2>/dev/null && ! grep -qE "$TERMINAL" "$log"; then
    echo "[test.sh] QEMU exited before the kernel reported anything (triple fault? see qemu.log)" >> "$log"
  fi
  if [ -n "$keys" ] && kill -0 "$pid" 2>/dev/null; then
    local t k
    for t in $(seq 1 50); do grep -q "keyboard ready" "$log" && break; sleep 0.2; done
    for k in $keys; do
      kill -0 "$pid" 2>/dev/null || break   # a key restarted or switched it off
      case $k in
        hold-*)                   # held since stage 2: wait until it's let go
          wait "$holder" 2>/dev/null; holder=
          continue ;;
        screendump)
          sleep 0.5               # let the screen settle
          rm -f "${log%/*}/screen.ppm"
          timeout 2 sh -c 'echo "screendump $1" > "$2"' _ "${log%/*}/screen.ppm" "$mon.in"
          for t in $(seq 1 25); do [ -s "${log%/*}/screen.ppm" ] && break; sleep 0.2; done
          sleep 0.3
          check_screendump "${log%/*}/screen.ppm" "$log"
          continue ;;
      esac
      # Bounded: opening the pipe would block forever if QEMU had just exited.
      timeout 2 sh -c 'echo "sendkey $1" > "$2"' _ "$k" "$mon.in"
      sleep 0.15
    done
    # -no-reboot makes QEMU exit on a restart as well as on power-off.
    for t in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    kill -0 "$pid" 2>/dev/null || echo "[test.sh] QEMU exited by itself" >> "$log"
  fi
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  if [ -n "$reader" ]; then kill "$reader" 2>/dev/null; wait "$reader" 2>/dev/null; fi
  if [ -n "$holder" ]; then kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null; fi
  if [ -n "$qmp_reader" ]; then kill "$qmp_reader" 2>/dev/null; wait "$qmp_reader" 2>/dev/null; fi
  if [ -n "$keys" ]; then rm -f "$mon.in" "$mon.out"; fi
  if [ -n "$hold" ]; then rm -f "$qmp.in" "$qmp.out"; fi
  if [ -n "$dirty" ]; then rm -f "$dirty"; fi
  sed -i 's/\r$//' "$log"   # the kernel ends serial lines with \r\n
}

hold_key() { # hold_key <QEMU key name> <QMP pipe> <serial log> <QEMU pid>, in the background
  # The key goes down once stage 2 has made its last disk and memory-map
  # calls (SeaBIOS lets interrupts in during those, which would hide a
  # stage 2 that never lets the BIOS see the keyboard itself), in time for
  # the quarter of a second it waits before looking at Shift. It repeats
  # every 2 to 3 ms, faster than a real keyboard (every 30 to 100), so a
  # byte is sure to come while the keyboard driver starts up, until half a
  # second after the driver is ready; half a second later it is let go
  # (sooner, and QEMU's 16-byte keyboard queue can still be full and drop
  # the release). A key held through boot once left the T430's keyboard
  # dead (Entry 19). The cases that hold one run QEMU's keyboard
  # controller with kbd-throttle, which spaces the bytes a millisecond
  # apart as a real keyboard's are; without it QEMU hands them over all at
  # once, and that bug almost never shows.
  local key=$1 in=$2 log=$3 pid=$4 t after=0
  local down='{"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":true,"key":{"type":"qcode","data":"KEY"}}}]}}'
  down=${down/KEY/$key}
  local up=${down/true/false}
  for t in $(seq 1 1000); do
    grep -q '\[stage2\] E820' "$log" && break
    kill -0 "$pid" 2>/dev/null || return
    sleep 0.01
  done
  exec 3> "$in"
  echo '{"execute":"qmp_capabilities"}' >&3
  local n=0
  while [ "$n" -lt 30000 ]; do   # bounded: a minute or two at most
    n=$((n + 1))
    echo "$down" >&3
    sleep 0.002
    [ $((n % 10)) -eq 0 ] || continue
    kill -0 "$pid" 2>/dev/null || return
    if [ "$after" -gt 0 ]; then
      after=$((after + 1)); [ "$after" -gt 15 ] && break   # about half a second
    elif grep -q 'keyboard ready' "$log"; then
      after=1
    fi
  done
  sleep 0.5
  echo "$up" >&3
  sleep 0.2
  exec 3>&-
}

check_screendump() { # check_screendump <ppm> <serial log>
  # QEMU's picture of the screen is as big as the mode the kernel says
  # stage 2 set: text mode shows as 720x400 (80x25 cells of 9x16 pixels).
  local ppm=$1 log=$2 want got
  want=$(sed -n 's/.*\[display\] VESA graphics \([0-9]*x[0-9]*\),.*/\1/p' "$log" | head -1)
  [ -z "$want" ] && grep -q '\[display\] text mode, 80x25' "$log" && want=720x400
  # A PPM starts "P6", then width and height, as text.
  got=$(head -c 40 "$ppm" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2 "x" $3 }')
  if [ -z "$got" ] || [ "$got" != "$want" ]; then
    echo "[test.sh] screendump is '${got:-missing}', the kernel reported '${want:-nothing}'" >> "$log"
    return
  fi
  if [ "$want" = 720x400 ]; then
    echo "[test.sh] screendump matches the mode: $got" >> "$log"
    return
  fi
  # Graphics: the 16-colour swatch on row 2 (from 36 cells before the
  # right edge, full-block glyphs in attributes 0-15) shows the VGA
  # palette exactly. This checks the kernel's pixel format, pitch and font
  # from outside: its own self-test can only compare with what it meant.
  local w=${got%x*} hdr i x y off seen bad=
  local palette=(000000 0000aa 00aa00 00aaaa aa0000 aa00aa aa5500 aaaaaa
                 555555 5555ff 55ff55 55ffff ff5555 ff55ff ffff55 ffffff)
  hdr=$(head -c 40 "$ppm" | head -3 | wc -c)   # "P6\nW H\n255\n"
  y=$((2 * 16 + 8))
  for i in $(seq 0 15); do
    x=$(((w / 8 - 36 + i) * 8 + 4))
    off=$((hdr + (y * w + x) * 3))
    seen=$(od -An -v -t x1 -j "$off" -N 3 "$ppm" | tr -d ' \n')
    [ "$seen" = "${palette[$i]}" ] || bad="${bad:+$bad, }colour $i is $seen, not ${palette[$i]}"
  done
  if [ -z "$bad" ]; then
    echo "[test.sh] screendump matches the mode: $got, and the colour swatch is the VGA palette" >> "$log"
  else
    echo "[test.sh] screendump is $got, but $bad" >> "$log"
  fi
}

check_scroll() { # check_scroll <serial log>: the scroll case's key presses
  # PageDown at the newest line stays at 0; PageUp twice is one page, then
  # two; PageDown is one again; sixteen more PageUps stop at the oldest row
  # kept (each step a page or less, the last ones equal); Enter on the
  # typed "hi" goes back to the newest line before printing it.
  # (Only notes after the keyboard driver started: a self-test scrolls too.)
  local views
  views=$(sed -n '/keyboard ready/,$ s/^\[console\] view \([0-9]*\) rows back$/\1/p' "$1" | tr '\n' ' ')
  local why
  why=$(echo "$views" | awk '{
      if (NF != 21) { print "expected 21 scroll notes, saw " NF; exit }
      p = $2
      if ($1 != 0)                  { print "PageDown at the end moved to " $1; exit }
      if (p <= 0)                   { print "PageUp did not move"; exit }
      if ($3 != 2 * p || $4 != p)   { print "pages: " $2 ", " $3 ", " $4; exit }
      for (i = 5; i <= 20; i++)
        if ($i < $(i-1) || $i - $(i-1) > p) { print "PageUp " i - 3 " went from " $(i-1) " to " $i; exit }
      if ($20 != $19)               { print "16 PageUps never reached the oldest row (" $20 ")"; exit }
      if ($20 >= 400)               { print "went back " $20 " rows; only 400 are kept"; exit }
      if ($21 != 0)                 { print "Enter left the view " $21 " rows back"; exit }
    }')
  if [ -z "$why" ] && [ -n "$views" ]; then
    echo "[test.sh] scrolling checked: $views" >> "$1"
  else
    echo "[test.sh] scrolling wrong: ${why:-no scroll notes} ($views)" >> "$1"
  fi
}

check_image() { # check_image <image>: the layout a real PC's BIOS relies on
  local img=$1 size sectors
  size=$(stat -c %s "$img"); sectors=$((size / 512))
  u() { od -An -t "u$2" -j "$1" -N "$2" "$img" | tr -d ' '; }  # u <offset> <bytes>
  [ $((size % 1048576)) -eq 0 ] || { echo "image is $size bytes, not a whole number of MiB"; return 1; }
  [ "$(u 510 2)" -eq $((0xAA55)) ] || { echo "no boot signature (0xAA55) at byte 510"; return 1; }
  [ "$(u 440 4)" -eq $((0x59455246)) ] || { echo "disk signature isn't 'FREY'"; return 1; }
  [ "$(u 446 1)" -eq $((0x80)) ] || { echo "partition 1 isn't marked active (0x80)"; return 1; }
  [ "$(u 450 1)" -eq $((0x7F)) ] || { echo "partition 1 type isn't 0x7F"; return 1; }
  [ "$(u 454 4)" -eq 1 ] || { echo "partition 1 doesn't start at sector 1"; return 1; }
  [ "$(u 458 4)" -eq $((sectors - 1)) ] || { echo "partition 1 is $(u 458 4) sectors; the image has $((sectors - 1)) after the MBR"; return 1; }
  [ -z "$(od -An -v -t x1 -j 462 -N 48 "$img" | tr -d ' 0\n')" ] \
    || { echo "partition entries 2-4 aren't empty"; return 1; }
  echo "$((size / 1048576)) MiB, MBR signature FREY, one active partition: sectors 1-$((sectors - 1))"
}

selected() { # is case $1 selected on the command line (or nothing selected)?
  [ "${#ARGS[@]}" -eq 0 ] && return 0
  local a; for a in "${ARGS[@]}"; do [ "$a" = "$1" ] && return 0; done
  return 1
}

ARGS=("$@")
pass=0; fail=0; results=()
echo "FreyaOS test suite ($QEMU, timeout ${TIMEOUT}s per boot)"

for entry in "${CASES[@]}"; do
  IFS='|' read -r name demo mem patterns extra keys <<< "$entry"
  selected "$name" || continue
  dir="$ROOT/$name"; mkdir -p "$dir"
  printf '  %-19s building...' "$name"

  # "bios-if-on" isn't a kernel demo: it builds stage2 to leave interrupts
  # on after its BIOS calls, as the T430's BIOS does (Entry 16).
  build=(FREYA_DEMO="$demo")
  [ "$demo" = bios-if-on ] && build=(FREYA_DEMO= BIOS_LEAVES_IF_ON=1)
  # "text-mode" builds stage2 to keep the BIOS's text mode (Entry 18).
  [ "$demo" = text-mode ] && build=(FREYA_DEMO= TEXT_MODE=1)
  if ! make -s OUTDIR="$dir" "${build[@]}" all > "$dir/build.log" 2>&1; then
    printf '\r  FAIL  %-19s build failed (see %s)\n' "$name" "$dir/build.log"
    tail -5 "$dir/build.log" | sed 's/^/          /'
    fail=$((fail + 1)); continue
  fi

  if [ "$name" = image ]; then
    if why=$(check_image "$dir/freya-os.img"); then
      printf '\r  PASS  %-19s %s\n' "$name" "$why"; pass=$((pass + 1))
    else
      printf '\r  FAIL  %-19s %s\n' "$name" "$why"; fail=$((fail + 1))
    fi
    continue
  fi

  printf '\r  %-19s booting... ' "$name"
  # shellcheck disable=SC2086  # extra is a list of separate arguments
  boot "$dir/freya-os.img" "$mem" "$dir/serial.log" "$keys" $extra
  [ "$name" = scroll ] && check_scroll "$dir/serial.log"

  missing=()
  # Split on ";;" exactly (IFS would split on every single ";").
  mapfile -t wanted < <(printf '%s;;%s\n' "$patterns" "$COMMON" | sed 's/;;/\n/g')
  for p in "${wanted[@]}"; do
    [ -z "$p" ] && continue
    grep -qE -- "$p" "$dir/serial.log" || missing+=("$p")
  done
  failed_lines=$(grep -E ': FAIL$' "$dir/serial.log")
  # A process's line starting in the middle of another line means two
  # writers were interleaved (Entry 17). Every case checks for it.
  interleaved=$(grep -nE '.\[user [0-9]+ ' "$dir/serial.log" | head -2)

  if [ "${#missing[@]}" -eq 0 ] && [ -z "$failed_lines" ] && [ -z "$interleaved" ]; then
    summary=$(grep -oE "$COMMON" "$dir/serial.log" | head -1)
    shown=$(grep -oE -- "${wanted[0]}" "$dir/serial.log" | head -1)
    printf '\r  PASS  %-19s %s | %s\n' "$name" "$summary" "$shown"
    pass=$((pass + 1))
  else
    printf '\r  FAIL  %-19s (serial log: %s)\n' "$name" "$dir/serial.log"
    for p in "${missing[@]}"; do echo "          missing: $p"; done
    [ -n "$failed_lines" ] && echo "$failed_lines" | sed 's/^/          /'
    [ -n "$interleaved" ] && echo "$interleaved" | sed 's/^/          interleaved output: /'
    echo "          last lines:"
    tail -4 "$dir/serial.log" | sed 's/^/            /'
    fail=$((fail + 1))
  fi
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
