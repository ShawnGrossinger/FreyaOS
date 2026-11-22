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
CASES=(
  "image||||"   # not booted: checks the disk image's layout (see check_image)
  "boot-128M||128M|init complete - boot thread done; scheduler and timer running;;self-test [0-9]+ ticks in 100 ms;;sleep 5 ticks woke after;;\\[user [0-9]+ 'hello'\\] Hello from ring 3!;;'probe' killed by vector 14 #PF;;\\[user [0-9]+ 'ipc'\\] printed for me by the log server;;driver handled 10"
  "bios-if-on|bios-if-on|128M|BIOS had left them ON after its last call; stage 2 turned them off\\): PASS;;self-tests: [0-9]+ passed, 0 failed"
  "dirty-ram||128M-dirty|self-tests: [0-9]+ passed, 0 failed;;init complete"
  "boot-4G||4G|physical map at 0xffff800000000000: 4096 MiB;;init complete - boot thread done; scheduler and timer running"
  "ud2|ud2|128M|vector 6 #UD Invalid Opcode"
  "page_fault|page_fault|128M|vector 14 #PF Page Fault;;cr2=0x0000000010000000;;page not present, read"
  "stack_overflow|stack_overflow|128M|vector 8 #DF Double Fault;;KERNEL STACK OVERFLOW;;running task: 0 'boot'"
  "task_stack_overflow|task_stack_overflow|128M|vector 8 #DF Double Fault;;TASK STACK OVERFLOW \(task [0-9]+ 'overflow-demo';;running task: [0-9]+ 'overflow-demo'"
  "smap|smap|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, read, supervisor mode;;kernel touched user memory|-cpu max"
  "smep|smep|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, instruction fetch, supervisor mode;;kernel touched user memory|-cpu max"
  "keyboard||128M|keyboard ready;;typed: hello World||delete end ctrl-delete alt-end ctrl-end alt-delete h e l l o spc shift-w o r l d ret"
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

  : > "$log"
  "$QEMU" -drive "format=raw,file=$img,if=ide,media=disk,index=0" "${memargs[@]}" \
          -display none "${monitor[@]}" -serial "file:$log" -no-reboot \
          -d int,cpu_reset -D "${log%/*}/qemu.log" "$@" </dev/null >/dev/null 2>&1 &
  local pid=$! ticks=0
  local reader=
  if [ -n "$keys" ]; then cat "$mon.out" > /dev/null & reader=$!; fi   # drain the monitor's replies
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
  if [ -n "$keys" ]; then rm -f "$mon.in" "$mon.out"; fi
  if [ -n "$dirty" ]; then rm -f "$dirty"; fi
  sed -i 's/\r$//' "$log"   # the kernel ends serial lines with \r\n
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
