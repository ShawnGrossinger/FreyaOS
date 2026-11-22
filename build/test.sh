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

# name | FREYA_DEMO | RAM | patterns that must all appear, separated by ;;
CASES=(
  "boot-128M||128M|init complete - idle, timer running;;self-test [0-9]+ ticks in 100 ms"
  "boot-4G||4G|physical map at 0xffff800000000000: 4096 MiB;;init complete - idle, timer running"
  "ud2|ud2|128M|vector 6 #UD Invalid Opcode"
  "page_fault|page_fault|128M|vector 14 #PF Page Fault;;cr2=0x0000000010000000;;page not present, read"
  "stack_overflow|stack_overflow|128M|vector 8 #DF Double Fault;;KERNEL STACK OVERFLOW"
  "nx|nx|128M|vector 14 #PF Page Fault;;protection violation, instruction fetch"
  "use_after_unmap|use_after_unmap|128M|vector 14 #PF Page Fault;;cr2=0xffffc00000100000;;page not present, read"
  "write_read_only|write_read_only|128M|vector 14 #PF Page Fault;;protection violation, write"
  "heap_double_free|heap_double_free|128M|\[heap\] FATAL: double free"
  "panic|panic|128M|\[kernel\] PANIC at src/main.rs:[0-9]+: FREYA_DEMO=panic"
)
COMMON="self-tests: [0-9]+ passed, 0 failed"

# A line on serial that means the kernel has stopped for good.
TERMINAL='init complete|\[EXCEPTION\] halted|FATAL|PANIC'

boot() { # boot <image> <ram> <serial log>
  local img=$1 mem=$2 log=$3
  local memargs=(-m "$mem")
  # Big guests: back RAM without reserving it up front (the guest only
  # touches a few MiB), so the host needn't have that much free.
  case $mem in *G) memargs=(-object "memory-backend-ram,id=ram0,size=$mem,reserve=off"
                             -machine memory-backend=ram0 -m "$mem") ;; esac

  : > "$log"
  "$QEMU" -drive "format=raw,file=$img,if=ide,media=disk,index=0" "${memargs[@]}" \
          -display none -monitor none -serial "file:$log" -no-reboot \
          -d int,cpu_reset -D "${log%/*}/qemu.log" </dev/null >/dev/null 2>&1 &
  local pid=$! ticks=0
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
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  sed -i 's/\r$//' "$log"   # the kernel ends serial lines with \r\n
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
  IFS='|' read -r name demo mem patterns <<< "$entry"
  selected "$name" || continue
  dir="$ROOT/$name"; mkdir -p "$dir"
  printf '  %-18s building...' "$name"

  if ! make -s OUTDIR="$dir" FREYA_DEMO="$demo" all > "$dir/build.log" 2>&1; then
    printf '\r  FAIL  %-18s build failed (see %s)\n' "$name" "$dir/build.log"
    tail -5 "$dir/build.log" | sed 's/^/          /'
    fail=$((fail + 1)); continue
  fi

  printf '\r  %-18s booting... ' "$name"
  boot "$dir/freya-os.img" "$mem" "$dir/serial.log"

  missing=()
  IFS=';;' read -r -a wanted <<< "$patterns;;$COMMON"
  for p in "${wanted[@]}"; do
    [ -z "$p" ] && continue
    grep -qE -- "$p" "$dir/serial.log" || missing+=("$p")
  done
  failed_lines=$(grep -E ': FAIL$' "$dir/serial.log")

  if [ "${#missing[@]}" -eq 0 ] && [ -z "$failed_lines" ]; then
    summary=$(grep -oE "$COMMON" "$dir/serial.log" | head -1)
    shown=$(grep -oE -- "${wanted[0]}" "$dir/serial.log" | head -1)
    printf '\r  PASS  %-18s %s | %s\n' "$name" "$summary" "$shown"
    pass=$((pass + 1))
  else
    printf '\r  FAIL  %-18s (serial log: %s)\n' "$name" "$dir/serial.log"
    for p in "${missing[@]}"; do echo "          missing: $p"; done
    [ -n "$failed_lines" ] && echo "$failed_lines" | sed 's/^/          /'
    echo "          last lines:"
    tail -4 "$dir/serial.log" | sed 's/^/            /'
    fail=$((fail + 1))
  fi
done

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
