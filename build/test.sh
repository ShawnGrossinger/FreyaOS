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
#        and logs whether its size is the mode the kernel reported;
#        "screendiff" takes another and logs whether the screen changed
#        since the last; "logsame" does the same for the log's rows below
#        its top one; "crashdump" checks the kernel's crash screen is
#        showing; "pause" waits a second and a half; and "hold-<key>"
#        (hold-shift, hold-ctrl) holds that key down, repeating like a real keyboard,
#        from stage 2 until just after the keyboard driver is ready (keys
#        after it are pressed once it has been let go). Keys wait for
#        "keyboard ready", or for a crash report to end. The mouse (Entry
#        22): "mouse:DX:DY" moves it (y down, as on screen), "wheel:N" turns
#        its wheel (positive is up), "button:MASK" sets its buttons (1 left,
#        2 right, 4 middle). "pointerdump" checks the pointer is drawn where
#        the console driver says; "shelldump" (Entry 23) that the line being
#        typed is on the last row with the cursor after it, and the shell's
#        last line above it in white, without the kernel's "[user N]".
#        Typed lines go to the shell (Entry 23): "e c h o spc h i ret" is
#        "echo hi", and the shell answers "hi".
CASES=(
  "image||||"   # not booted: checks the disk image's layout (see check_image)
  "boot-128M||128M|init complete - boot thread done; scheduler and timer running;;\\[user 3 'init'\\] started the console driver \\(process 4\\), the PS/2 driver \\(process 5\\), the mouse driver \\(process 6\\) and the shell \\(process 7\\);;\\[user 7 'shell'\\] shell ready: type help;;\\[console\\] process [0-9]+ \\('console'\\) draws the screen from here on;;\\[display\\] VESA graphics [0-9]+x[0-9]+, 32-bit;;reads back pixel for pixel \\(128 pixels, [0-9]+ lit, 0 wrong\\); framebuffer mapped write-combining \\(PAT entry 4\\): PASS;;self-test [0-9]+ ticks in 100 ms;;sleep 5 ticks woke after;;\\[user [0-9]+ 'hello'\\] Hello from ring 3!;;'probe' killed by vector 14 #PF;;\\[user [0-9]+ 'ipc'\\] printed for me by the log server;;driver handled 10"
  "bios-if-on|bios-if-on|128M|BIOS had left them ON after its disk calls; stage 2 turned them off\\): PASS;;self-tests: [0-9]+ passed, 0 failed"
  "dirty-ram||128M-dirty|self-tests: [0-9]+ passed, 0 failed;;init complete"
  "boot-4G||4G|physical map at 0xffff800000000000: 4096 MiB;;init complete - boot thread done; scheduler and timer running"
  "ud2|ud2|128M|vector 6 #UD Invalid Opcode"
  "page_fault|page_fault|128M|vector 14 #PF Page Fault;;cr2=0x0000000010000000;;page not present, read;;crash screen: red at the top||crashdump"
  "stack_overflow|stack_overflow|128M|vector 8 #DF Double Fault;;KERNEL STACK OVERFLOW;;running task: 0 'boot'"
  "task_stack_overflow|task_stack_overflow|128M|vector 8 #DF Double Fault;;TASK STACK OVERFLOW \(task [0-9]+ 'overflow-demo';;running task: [0-9]+ 'overflow-demo'"
  "smap|smap|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, read, supervisor mode;;kernel touched user memory|-cpu max"
  "smep|smep|128M|vector 14 #PF Page Fault;;cr2=0x0000000040000000;;protection violation, instruction fetch, supervisor mode;;kernel touched user memory|-cpu max"
  "keyboard||128M|keyboard ready;;\\[console note\\] input row: freya> echo hello World$;;'shell'\\] freya> echo hello World$;;'shell'\\] hello World$||delete end ctrl-delete alt-end ctrl-end alt-delete ctrl-x alt-y e c h o spc h e l l o spc shift-w o r l d ret"
  "scroll||128M|scrolling checked: [0-9 ]+;;'shell'\\] hi$;;screendump matches the mode: [0-9]+x[0-9]+, and the colour swatch is the VGA palette||pgdn pgup pgup pgdn pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup e c h o spc h i ret screendump"
  "text-mode|text-mode|128M|text mode, 80x25: built with TEXT_MODE=1;;a character put in the last cell of text mode is in VGA memory .*: PASS;;\\[console note\\] view 5 rows back;;\\[console note\\] view 10 rows back;;'shell'\\] hi$;;screendump matches the mode: 720x400||pgup pgup screendump e c h o spc h i ret"
  "edid-1366||128M|display's own resolution 1366x768 \\(EDID\\);;VESA graphics 1280x768, 32-bit;;screendump matches the mode: 1280x768, and the colour swatch is the VGA palette;;the screen changed since the last screendump|-vga none -device VGA,xres=1366,yres=768|screendump pause screendiff"
  "edid-1440||128M|display's own resolution 1440x900 \\(EDID\\);;VESA graphics 1440x900, 32-bit|-vga none -device VGA,xres=1440,yres=900|"
  "no-edid||128M|display's own resolution unknown \\(no EDID: modes up to 1024x768\\);;VESA graphics 1024x768, 32-bit;;screendump matches the mode: 1024x768, and the colour swatch|-vga none -device VGA,edid=off|screendump"
  "cirrus-24bit||128M|VESA graphics 1024x768, 24-bit;;reads back pixel for pixel \\(128 pixels, [0-9]+ lit, 0 wrong\\);;screendump matches the mode: 1024x768, and the colour swatch|-vga cirrus|screendump"
  "shift||128M|display: status 0x02;;text mode, 80x25: Shift held while booting;;a character put in the last cell of text mode is in VGA memory .*: PASS;;'shell'\\] hi$|-global i8042.kbd-throttle=on|hold-shift e c h o spc h i ret"
  "held-key||128M|VESA graphics [0-9]+x[0-9]+;;'shell'\\] hi$|-global i8042.kbd-throttle=on|hold-ctrl e c h o spc h i ret"
  "restart||128M|\\[power\\] Restarting;;restart through the keyboard controller;;QEMU exited by itself||ctrl-alt-delete"
  "shutdown||128M|switching the firmware to ACPI mode;;\\[power\\] ACPI soft off \\(S5\\);;QEMU exited by itself||ctrl-alt-end"
  "restart-q35||128M|restart through the ACPI reset register;;QEMU exited by itself|-machine q35|ctrl-alt-delete"
  "nx|nx|128M|vector 14 #PF Page Fault;;protection violation, instruction fetch"
  "use_after_unmap|use_after_unmap|128M|vector 14 #PF Page Fault;;cr2=0xffffc00000100000;;page not present, read"
  "write_read_only|write_read_only|128M|vector 14 #PF Page Fault;;protection violation, write"
  "heap_double_free|heap_double_free|128M|\[heap\] FATAL: double free"
  "panic|panic|128M|\[kernel\] PANIC at src/main.rs:[0-9]+: FREYA_DEMO=panic;;crash screen: red at the top||crashdump"
  "vbe-lies|vbe-lies|128M|display's own resolution 1280x800 \\(EDID\\);;VESA graphics 800x600, 32-bit;;screendump matches the mode: 800x600, and the colour swatch is the VGA palette|-vga none -device VGA,xres=1280,yres=800|screendump"
  "kill-console|kill_console|128M|\\[demo\\] console driver killed;;ended: the kernel draws the screen again;;\\[user [0-9]+ 'init'\\] the console driver \\(process [0-9]+\\) ended: started it again as process [0-9]+ \\(1 of 3\\);;started again: it draws and scrolls;;started again: the shell sent it the line being typed;;'shell'\\] hi$;;screendump matches the mode: [0-9]+x[0-9]+, and the colour swatch is the VGA palette||screendump pause pgup e c h o spc h i ret"
  "kill-console-often|kill_console_often|128M|\\[demo\\] console driver killed \\(4 of 4\\);;started it again as process [0-9]+ \\(3 of 3\\);;started again 3 times already, so it stays stopped;;'shell'\\] hi$;;no console notes after it stayed stopped;;screendump matches the mode: [0-9]+x[0-9]+, and the colour swatch is the VGA palette;;the screen changed since the last screendump||screendump pause pause pgup e c h o spc h i ret pause screendiff"
  "kill-init|kill_init|128M|\\[demo\\] init killed;;\\[demo\\] console driver killed;;ended: the kernel draws the screen again;;'shell'\\] hi$;;no console notes after it ended;;screendump matches the mode: [0-9]+x[0-9]+, and the colour swatch is the VGA palette;;the screen changed since the last screendump||screendump pause pgup e c h o spc h i ret pause screendiff"
  "mouse||128M|mouse ready: 4-byte packets, with a wheel;;mouse checked: ;;'shell'\\] hi$;;the pointer is drawn where the console says||pause mouse:80:48 button:1 button:0 wheel:1 wheel:-1 button:4 mouse:0:-64 button:0 mouse:0:64 mouse:127:127 mouse:127:127 mouse:127:127 mouse:127:127 mouse:127:127 mouse:-40:-30 e c h o spc h i ret pointerdump"
  "shell||128M|shell ready: type help;;'shell'\\]   echo TEXT  print TEXT$;;'shell'\\] freya> echo two$;;'shell'\\]   3  init          29  waiting for a message on endpoint [0-9]+$;;'shell'\\]   4  console        6  waiting for a message on endpoint [0-9]+$;;'shell'\\]   5  ps2           10  waiting for a message on endpoint [0-9]+$;;'shell'\\]   6  mouse          4  waiting for a message on endpoint [0-9]+$;;'shell'\\]   7  shell          9  running$;;'shell'\\] [0-9]+ tasks: 5 processes, [0-9]+ in the kernel$;;'shell'\\] slot  0  Console: writes to the log$;;'shell'\\] slot  1  Endpoint [0-9]+: receive$;;'shell'\\] slot  2  Endpoint [0-9]+: send, badge 4$;;'shell'\\] slot  3  Endpoint [0-9]+: send, badge 4$;;'shell'\\] slot  4  Info: reads uptime, memory, the tasks$;;'shell'\\] slot  5  Power: restarts, switches off$;;'shell'\\] slot  6  Factory: starts hello, spin, crash, ticker, relay, hog$;;'shell'\\] slot  7  Endpoint [0-9]+: send$;;'shell'\\] slot  8  Budget: 4096 KiB of memory, 0 KiB in use$;;'shell'\\] 9 of 32 slots hold a capability;;'shell'\\] up 0 h 0 min [0-9]+ s \\([0-9]+ timer ticks, 100 a second\\);;'shell'\\] memory: [0-9]+ MiB free of 127 MiB usable;;'shell'\\] foo: no such command \\(help lists them\\)$;;shell checked: ;;the line being typed is on the last row||h e l p ret e c h o o backspace spc o n e ret x y z esc e c h o spc t w o ret up up ret p s ret c a p s ret u p t i m e ret m e m ret f o o ret e c h o shelldump"
  "programs||128M|'shell'\\] started spin as process 8$;;'shell'\\]   8  spin           1  ready to run$;;'shell'\\] process 5: this shell holds no capability for it;;'shell'\\] stopping spin \\(process 8\\)$;;'shell'\\] spin \\(process 8\\) ended: stopped \\(kill\\)$;;\\[user [0-9]+ 'hello'\\] Hello from ring 3!$;;'shell'\\] hello \\(process [0-9]+\\) ended: exit code 19$;;\\[user\\] task [0-9]+ 'crash' killed by vector 14 #PF Page Fault .*cr2=0xffff800000100000 \\(protection violation, write, user mode\\)$;;'shell'\\] crash \\(process [0-9]+\\) ended: stopped by the CPU: exception 14 \\(page fault\\)$;;'shell'\\] console: this shell's Factory capability doesn't cover it;;'shell'\\] no programs started from this shell are running$;;programs checked: ||r u n spc s p i n ret pause p s ret j o b s ret k i l l spc 5 ret k i l l spc 8 ret pause r u n spc s p i n ret r u n spc s p i n ret k i l l spc s p i n ret pause r u n spc h e l l o ret pause r u n spc c r a s h ret pause r u n spc c o n s o l e ret j o b s ret p s ret"
  "revoke||128M|'shell'\\] started ticker as process 8$;;\\[user 8 'ticker'\\] ticker: tick 1$;;'shell'\\] slot  0  Console: writes to the log; 2 copies made from it$;;'shell'\\] slot  9  Console: writes to the log; lent to ticker \\(process 8\\); 1 copy made from it$;;'shell'\\] slot 10  Process 8 \\(ticker\\): asks how it is doing, stops it$;;'shell'\\] took back what this shell lent ticker \\(process 8\\): 1 capability$;;'shell'\\] ticker \\(process 8\\) ended: exit code -2 \\(no capability\\)$;;'relay'\\] relay: started ticker as process [0-9]+, with my console$;;'shell'\\] +[0-9]+  relay +2  ;;'shell'\\] +[0-9]+  ticker +1  ;;'shell'\\] slot  9  Console: writes to the log; lent to relay \\(process [0-9]+\\); 2 copies made from it$;;'shell'\\] slot  6  Factory: starts hello, spin, crash, ticker, relay, hog; 2 copies made from it$;;'shell'\\] slot 10  Factory: starts ticker; lent to relay \\(process [0-9]+\\); 1 copy made from it$;;'shell'\\] took back what this shell lent relay \\(process [0-9]+\\): 3 capabilities$;;'shell'\\] relay \\(process [0-9]+\\) ended: exit code -2 \\(no capability\\)$;;'shell'\\] relay \\(process [0-9]+\\) ended: stopped \\(kill\\); took back 1 capability it had passed on$;;'shell'\\] process 3: this shell lent it nothing;;'shell'\\] 9 of 32 slots hold a capability;;revoke checked: ||r u n spc t i c k e r ret pause pause c a p s ret r e v o k e spc 8 ret pause pause r u n spc r e l a y ret pause p s ret c a p s ret r e v o k e spc r e l a y ret pause pause r u n spc r e l a y ret pause k i l l spc r e l a y ret pause pause p s ret r e v o k e spc 3 ret c a p s ret"
  "heap||128M|'shell'\\] started hog as process 8$;;\\[user 8 'hog'\\] hog: holding 1024 KiB$;;\\[user 8 'hog'\\] hog: my budget won't give more; holding 1024 KiB, and reading it back every second$;;'shell'\\] slot  8  Budget: 4096 KiB of memory, 1024 KiB in use; 2 copies made from it$;;'shell'\\] slot 10  Budget: 1024 KiB of memory, 1024 KiB in use; lent to hog \\(process 8\\); 1 copy made from it$;;'shell'\\] took back what this shell lent hog \\(process 8\\): 2 capabilities$;;\\[user\\] task 8 'hog' killed by vector 14 #PF Page Fault at rip=0x[0-9a-f]+ cr2=0x0000410000000000 \\(page not present, read, user mode\\);;'shell'\\] hog \\(process 8\\) ended: stopped by the CPU: exception 14 \\(page fault\\)$;;'shell'\\] slot  8  Budget: 4096 KiB of memory, 4096 KiB in use; 10 copies made from it$;;'shell'\\] hog \\(process [0-9]+\\) ended: stopped \\(kill\\)$;;'shell'\\] slot  8  Budget: 4096 KiB of memory, 0 KiB in use$;;'shell'\\] 9 of 32 slots hold a capability;;heap checked: ||m e m ret r u n spc h o g ret pause pause c a p s ret r e v o k e spc h o g ret pause pause r u n spc h o g ret r u n spc h o g ret r u n spc h o g ret r u n spc h o g ret r u n spc h o g ret pause pause pause c a p s ret k i l l spc h o g ret pause pause c a p s ret m e m ret"
  "kill-shell|kill_shell|128M|\\[demo\\] shell killed;;\\[user 3 'init'\\] the shell \\(process 7\\) ended: started it again as process [0-9]+ \\(1 of 3\\);;'shell'\\] hi$;;the shell started again answered||pause e c h o spc h i ret"
  "log-flood|log_flood|128M|flood 600;;tick 1;;flood checked: ;;screendump matches the mode: 1280x800, and the colour swatch is the VGA palette;;the log below the bar did not change||pause pgup screendump pause logsame pgdn pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup pgup"
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
  # What the harness itself finds while QEMU runs goes here, and into the
  # serial log only once QEMU has stopped: QEMU writes the log at its own
  # offset, so a line appended meanwhile could be overwritten (Entry 20).
  local notes=${log%/*}/notes.log
  : > "$notes"
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
      echo "[test.sh] TIMEOUT: no terminal line after ${TIMEOUT}s" >> "$notes"
      break
    fi
  done
  if ! kill -0 "$pid" 2>/dev/null && ! grep -qE "$TERMINAL" "$log"; then
    echo "[test.sh] QEMU exited before the kernel reported anything (triple fault? see qemu.log)" >> "$log"
  fi
  if [ -n "$keys" ] && kill -0 "$pid" 2>/dev/null; then
    local t k
    for t in $(seq 1 50); do grep -qE "keyboard ready|\[EXCEPTION\] halted|PANIC" "$log" && break; sleep 0.2; done
    for k in $keys; do
      kill -0 "$pid" 2>/dev/null || break   # a key restarted or switched it off
      case $k in
        hold-*)                   # held since stage 2: wait until it's let go
          wait "$holder" 2>/dev/null; holder=
          continue ;;
        pause) sleep 1.5; continue ;;
        mouse:*|wheel:*|button:*)  # the PS/2 mouse, through the monitor (Entry 22)
          local cmd
          case $k in
            mouse:*) cmd="mouse_move $(echo "${k#mouse:}" | tr ':' ' ')" ;;
            wheel:*) cmd="mouse_move 0 0 ${k#wheel:}" ;;
            button:*) cmd="mouse_button ${k#button:}" ;;
          esac
          timeout 2 sh -c 'echo "$1" > "$2"' _ "$cmd" "$mon.in"
          sleep 0.3
          continue ;;
        screendiff)
          take_screendump "${log%/*}/screen2.ppm" "$mon.in"
          if [ ! -s "${log%/*}/screen.ppm" ] || [ ! -s "${log%/*}/screen2.ppm" ]; then
            echo "[test.sh] screendiff: a screendump is missing" >> "$notes"
          elif cmp -s "${log%/*}/screen.ppm" "${log%/*}/screen2.ppm"; then
            echo "[test.sh] the screen did not change since the last screendump" >> "$notes"
          else
            echo "[test.sh] the screen changed since the last screendump" >> "$notes"
          fi
          continue ;;
        crashdump)
          take_screendump "${log%/*}/screen.ppm" "$mon.in"
          check_crash_screen "${log%/*}/screen.ppm" "$notes"
          continue ;;
        pointerdump)
          take_screendump "${log%/*}/screen.ppm" "$mon.in"
          check_pointer "${log%/*}/screen.ppm" "$log" "$notes"
          continue ;;
        shelldump)
          take_screendump "${log%/*}/screen.ppm" "$mon.in"
          check_shell_screen "${log%/*}/screen.ppm" "$log" "$notes"
          continue ;;
        logsame)
          take_screendump "${log%/*}/screen2.ppm" "$mon.in"
          check_log_same "${log%/*}/screen.ppm" "${log%/*}/screen2.ppm" "$notes"
          continue ;;
        screendump)
          take_screendump "${log%/*}/screen.ppm" "$mon.in"
          check_screendump "${log%/*}/screen.ppm" "$log" "$notes"
          continue ;;
      esac
      # Bounded: opening the pipe would block forever if QEMU had just exited.
      timeout 2 sh -c 'echo "sendkey $1" > "$2"' _ "$k" "$mon.in"
      sleep 0.15
    done
    # -no-reboot makes QEMU exit on a restart as well as on power-off.
    for t in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    kill -0 "$pid" 2>/dev/null || echo "[test.sh] QEMU exited by itself" >> "$notes"
  fi
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  cat "$notes" >> "$log"
  rm -f "$notes"
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

check_screendump() { # check_screendump <ppm> <serial log> <notes file>
  # QEMU's picture of the screen is as big as the mode the kernel says
  # stage 2 set: text mode shows as 720x400 (80x25 cells of 9x16 pixels).
  local ppm=$1 log=$2 out=$3 want got
  want=$(sed -n 's/.*\[display\] VESA graphics \([0-9]*x[0-9]*\),.*/\1/p' "$log" | head -1)
  [ -z "$want" ] && grep -q '\[display\] text mode, 80x25' "$log" && want=720x400
  # A PPM starts "P6", then width and height, as text.
  got=$(head -c 40 "$ppm" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2 "x" $3 }')
  if [ -z "$got" ] || [ "$got" != "$want" ]; then
    echo "[test.sh] screendump is '${got:-missing}', the kernel reported '${want:-nothing}'" >> "$out"
    return
  fi
  if [ "$want" = 720x400 ]; then
    echo "[test.sh] screendump matches the mode: $got" >> "$out"
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
    echo "[test.sh] screendump matches the mode: $got, and the colour swatch is the VGA palette" >> "$out"
  else
    echo "[test.sh] screendump is $got, but $bad" >> "$out"
  fi
}

take_screendump() { # take_screendump <ppm> <monitor pipe>: QEMU's picture of the screen
  sleep 0.5                 # let the screen settle
  rm -f "$1"
  # Bounded: opening the pipe would block forever if QEMU had just exited.
  timeout 2 sh -c 'echo "screendump $1" > "$2"' _ "$1" "$2"
  local t
  for t in $(seq 1 25); do [ -s "$1" ] && break; sleep 0.2; done
  sleep 0.3                 # QEMU may still be writing it
}

check_log_same() { # check_log_same <earlier ppm> <later ppm> <notes file>
  # The log's rows below its top one (where the scrolled-back bar says how
  # far back the view is), in two screendumps taken while scrolled back:
  # the same pixels mean new lines left the view where it was (Entry 20).
  local a=$1 b=$2 out=$3 w hdr off
  w=$(head -c 40 "$a" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2 }')
  if [ -z "$w" ] || [ ! -s "$b" ]; then
    echo "[test.sh] logsame: a screendump is missing" >> "$out"
    return
  fi
  hdr=$(head -c 40 "$a" | head -3 | wc -c)
  off=$((hdr + 19 * 16 * w * 3))   # row 19: the log starts at row 18
  if cmp -s -i "$off:$off" "$a" "$b"; then
    echo "[test.sh] the log below the bar did not change" >> "$out"
  else
    echo "[test.sh] the log below the bar changed" >> "$out"
  fi
}

check_pointer() { # check_pointer <ppm> <serial log> <notes file>
  # The cell the console driver last said the pointer is in must be drawn
  # with its colours swapped (Entry 22). The mouse case leaves it on a
  # blank cell of the log: black otherwise, swapped the colour of the
  # row's text, grey (0xaaaaaa), or white (0xffffff) on a row of the
  # shell's (Entry 23). Its top-left pixel is background in every glyph of
  # the font.
  local ppm=$1 log=$2 out=$3 w hdr col row off seen
  read -r col row < <(grep -a '^\[console note\] pointer at column' "$log" | tail -1 | sed 's/.*column \([0-9]*\), row \([0-9]*\);.*/\1 \2/')
  w=$(head -c 40 "$ppm" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2 }')
  if [ -z "$w" ] || [ -z "$row" ]; then
    echo "[test.sh] pointerdump: no screendump, or no pointer note" >> "$out"
    return
  fi
  hdr=$(head -c 40 "$ppm" | head -3 | wc -c)
  off=$((hdr + (row * 16 * w + col * 8) * 3))
  seen=$(od -An -v -t x1 -j "$off" -N 3 "$ppm" | tr -d ' \n')
  if [ "$seen" = aaaaaa ] || [ "$seen" = ffffff ]; then
    echo "[test.sh] the pointer is drawn where the console says (column $col, row $row)" >> "$out"
  else
    echo "[test.sh] the pointer's cell (column $col, row $row) is $seen, not swapped" >> "$out"
  fi
}

check_mouse() { # check_mouse <serial log>: the mouse case's moves
  # The pointer moves over a field of 640 x 400 counts in every mode, laid
  # over the screen's cells (Entry 23): on 1280x800's 160 x 50 cells, a
  # cell is 4 counts wide and 8 high. From the middle (320, 200: cell 80,
  # 25): 80 right and 48 down is (400, 248), column 100, row 31; left
  # pressed and let go there; the wheel up a notch (3 rows back) and down
  # again (0); the middle button held and 64 up (4 rows back, the pointer
  # staying put); let go, 64 down (312: row 39); five steps of 127 right
  # and down, past the corner (159, 49); then 40 left and 30 up (599,
  # 369: column 149, row 46). (QEMU's PS/2 mouse sends at most 127 a
  # packet and only a few packets per move; the rest waits for the next
  # move, so the case moves in steps.)
  local pointers views why
  pointers=$(sed -n '/mouse ready/,$ s/^\[console note\] pointer at column \([0-9]*\), row \([0-9]*\); buttons: \(.*\)$/\1,\2,\3/p' "$1" | tr ' ' '+' | tr '\n' ' ')
  views=$(sed -n '/mouse ready/,$ s/^\[console note\] view \([0-9]*\) rows back.*/\1/p' "$1" | tr '\n' ' ')
  why=$(awk -v p="$pointers" -v v="$views" 'BEGIN {
      n = split(p, at, " "); m = split(v, view, " ")
      if (at[1] != "100,31,none") { print "first: " at[1]; exit }
      if (at[2] != "100,31,left" || at[3] != "100,31,none") { print "click: " at[2] " " at[3]; exit }
      if (m < 3 || view[1] != 3 || view[2] != 0 || view[3] != 4) { print "scrolling: " v; exit }
      if (at[4] != "100,31,middle" || at[5] != "100,31,none" || at[6] != "100,39,none") { print "middle: " at[4] " " at[5] " " at[6]; exit }
      corner = 0; for (i = 7; i <= n; i++) if (at[i] == "159,49,none") corner = 1
      if (!corner) { print "never reached the corner (159, 49)"; exit }
      if (at[n] != "149,46,none") { print "last: " at[n]; exit }
      print "ok"
    }')
  if [ "$why" = ok ]; then
    echo "[test.sh] mouse checked: ${pointers}| views ${views}" >> "$1"
  else
    echo "[test.sh] mouse wrong: $why ($pointers | $views)" >> "$1"
  fi
}

cell_pixels() { # cell_pixels <ppm> <header bytes> <width> <row> <col>: a cell's 8 x 16 pixels, in hex
  local y
  for y in $(seq 0 15); do
    od -An -v -t x1 -j $(($2 + ((($4 * 16 + y) * $3) + $5 * 8) * 3)) -N 24 "$1"
  done | tr -d ' \n'
}

check_shell_screen() { # check_shell_screen <ppm> <serial log> <notes file>
  # The shell case ends with "echo" typed (Entry 23). The screen's last
  # row must be the line the console driver last noted ("input row:
  # freya> echo"), with the cursor after it: grey (0xaaaaaa) at its top-left
  # pixel. Two rows up, above the log's row in progress, is the shell's
  # last line, "foo: no such command": drawn without the kernel's "[user N
  # 'shell'] " and in bright white, its first cell is the same 'f', pixel
  # for pixel, as the prompt's on the last row, and has white (0xffffff)
  # in it.
  local ppm=$1 log=$2 out=$3 w h hdr text rows off seen prompt last
  # (Read while QEMU runs: its lines still end in \r.)
  text=$(grep -a '^\[console note\] input row: ' "$log" | tail -1 | sed 's/^\[console note\] input row: //; s/\r$//')
  read -r w h < <(head -c 40 "$ppm" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2, $3 }')
  if [ -z "$w" ] || [ -z "$text" ]; then
    echo "[test.sh] shelldump: no screendump, or no input row note" >> "$out"
    return
  fi
  hdr=$(head -c 40 "$ppm" | head -3 | wc -c)
  rows=$((h / 16))
  off=$((hdr + ((rows - 1) * 16 * w + ${#text} * 8) * 3))
  seen=$(od -An -v -t x1 -j "$off" -N 3 "$ppm" | tr -d ' \n')
  prompt=$(cell_pixels "$ppm" "$hdr" "$w" $((rows - 1)) 0)
  last=$(cell_pixels "$ppm" "$hdr" "$w" $((rows - 3)) 0)
  if [ "$text" != "freya> echo" ]; then
    echo "[test.sh] shelldump: the input row is '$text', not 'freya> echo'" >> "$out"
  elif [ "$seen" != aaaaaa ]; then
    echo "[test.sh] shelldump: the cursor's cell (column ${#text}) is $seen, not grey" >> "$out"
  elif [ "$prompt" != "$last" ] || [[ $last != *ffffff* ]]; then
    echo "[test.sh] shelldump: the shell's last line doesn't start with a white 'f' like the prompt's" >> "$out"
  else
    echo "[test.sh] the line being typed is on the last row, the cursor after it, and the shell's lines are white and unprefixed" >> "$out"
  fi
}

check_shell() { # check_shell <serial log>: the shell case's editing keys
  # "echoo", Backspace, " one": "echo one", once. "xyz", Esc, "echo two":
  # no xyz. Up twice goes back past "echo two" to "echo one", which runs
  # again: two of each "freya> echo one" and "one", one "freya> echo two".
  local one ones two xyz
  one=$(grep -c "^\[user [0-9]* 'shell'\] freya> echo one$" "$1")
  ones=$(grep -c "^\[user [0-9]* 'shell'\] one$" "$1")
  two=$(grep -c "^\[user [0-9]* 'shell'\] freya> echo two$" "$1")
  xyz=$(grep -c "^\[user [0-9]* 'shell'\] .*xyz" "$1")
  if [ "$one" = 2 ] && [ "$ones" = 2 ] && [ "$two" = 1 ] && [ "$xyz" = 0 ]; then
    echo "[test.sh] shell checked: Backspace, Esc, and Up twice to the line before last" >> "$1"
  else
    echo "[test.sh] shell wrong: 'echo one' entered $one times (answered $ones), 'echo two' $two, lines with xyz $xyz" >> "$1"
  fi
}

check_programs() { # check_programs <serial log>: the programs case
  # Three spins stopped through the shell's Process capabilities (kill 8,
  # then kill spin for two at once); while the first ran, the shell held
  # two more capabilities (11: its Process capability, Entry 24, and the
  # console lent to it, Entry 25), and at the end, with every program gone,
  # it is back to its 9 (8 before Entry 26 gave it a Budget): each one let
  # go of.
  local stopped first last
  stopped=$(grep -c "^\\[user [0-9]* 'shell'\\] spin (process [0-9]*) ended: stopped (kill)$" "$1")
  first=$(grep "^\\[user [0-9]* 'shell'\\]   7  shell " "$1" | head -1 | awk '{print $6}')
  last=$(grep "^\\[user [0-9]* 'shell'\\]   7  shell " "$1" | tail -1 | awk '{print $6}')
  if [ "$stopped" = 3 ] && [ "$first" = 11 ] && [ "$last" = 9 ]; then
    echo "[test.sh] programs checked: 3 spins stopped; the shell held 11 capabilities with one running, 9 after" >> "$1"
  else
    echo "[test.sh] programs wrong: $stopped spins stopped; the shell held $first, then $last capabilities" >> "$1"
  fi
}

check_revoke() { # check_revoke <serial log>: the revoke case (Entry 25)
  # After each take-back (revoke ticker, revoke relay, the relay killed),
  # no ticker writes another line until a relay is started again: what
  # the shell lent is gone from every process that had it. And the last ps
  # has no ticker or relay left, with the shell back to its 9 capabilities.
  local late left shell
  # Only the shell's and the tickers' own lines: a console driver note
  # quotes rows of the screen, and can hold a take-back and a tick at once.
  late=$(awk '/^\[user [0-9]+ '"'"'shell'"'"'\] .*took back /{quiet=1} /^\[user [0-9]+ '"'"'shell'"'"'\] started relay as process/{quiet=0} quiet && /^\[user [0-9]+ '"'"'ticker'"'"'\] ticker: tick/{n++} END{print n+0}' "$1")
  left=$(awk '/'"'"'shell'"'"'\] freya> ps$/{block=""} {block=block"\n"$0} END{print block}' "$1" | grep -cE "^\[user [0-9]+ 'shell'\] +[0-9]+  (ticker|relay) ")
  shell=$(grep "^\[user [0-9]* 'shell'\]   7  shell " "$1" | tail -1 | awk '{print $6}')
  if [ "$late" = 0 ] && [ "$left" = 0 ] && [ "$shell" = 9 ]; then
    echo "[test.sh] revoke checked: no tick after a take-back; the last ps has no ticker or relay; the shell holds 9 again" >> "$1"
  else
    echo "[test.sh] revoke wrong: $late ticks after a take-back; $left tickers or relays left; the shell holds $shell" >> "$1"
  fi
}

check_heap() { # check_heap <serial log>: the heap case (Entry 26)
  # The five hogs started after the first was revoked share the shell's
  # 4 MiB: each says what it holds once its budget won't give more, and
  # together that is 4096 KiB, none over its own 1024. And once they are
  # killed, the machine has as many free pages as before the first hog.
  local holds most pages
  holds=$(awk '/^\[user [0-9]+ '"'"'shell'"'"'\] took back what this shell lent hog/{after=1} after && /^\[user [0-9]+ '"'"'hog'"'"'\] hog: my budget won.t give more; holding/{for(i=1;i<=NF;i++) if($i=="holding"){n++; sum+=$(i+1); if($(i+1)>most) most=$(i+1)}} END{print n+0, sum+0, most+0}' "$1")
  pages=$(grep -oE "^\[user [0-9]+ 'shell'\] memory: .* \(([0-9]+) of" "$1" | grep -oE "\(([0-9]+)" | tr -d '(' | sort -u | wc -l)
  if [ "$holds" = "5 4096 1024" ] && [ "$pages" = 1 ]; then
    echo "[test.sh] heap checked: five hogs held 4096 KiB between them, none over 1024; as many pages free after as before" >> "$1"
  else
    echo "[test.sh] heap wrong: hogs (how many, KiB in all, most): $holds; distinct free-page counts: $pages" >> "$1"
  fi
}

check_crash_screen() { # check_crash_screen <ppm> <notes file>
  # The crash screen's top rows are white on red. On a screen the console
  # driver had drawn, that shows the kernel took it back (Entry 20). The
  # pixel looked at is in row 0, two cells from the right edge: background.
  local ppm=$1 out=$2 w hdr off seen
  w=$(head -c 40 "$ppm" 2>/dev/null | tr '\n' ' ' | awk '$1 == "P6" { print $2 }')
  if [ -z "$w" ]; then
    echo "[test.sh] crash screen: no screendump" >> "$out"
    return
  fi
  hdr=$(head -c 40 "$ppm" | head -3 | wc -c)
  off=$((hdr + (4 * w + (w / 8 - 2) * 8 + 4) * 3))
  seen=$(od -An -v -t x1 -j "$off" -N 3 "$ppm" | tr -d ' \n')
  if [ "$seen" = aa0000 ]; then
    echo "[test.sh] crash screen: red at the top" >> "$out"
  else
    echo "[test.sh] crash screen: the top is $seen, not red" >> "$out"
  fi
}

check_flood() { # check_flood <serial log>: the log-flood case's key presses
  # The console driver's notes, after each key: view B rows back of K
  # kept, and the view's top row, its bottom row and the newest finished
  # row (the first 56 characters of each). The demo printed 600 lines
  # ("flood N"), then one every quarter second ("tick N").
  #   1st (PageUp): one page back, B1 rows; its top row T1.
  #   3rd (PageDown, PageUp, after a pause in which ticks arrived): back
  #       further than B1, because the view stayed on the same rows while
  #       new ones came in below; its top row still T1.
  #   last (17 more PageUps): at the oldest of 400 rows kept: B = 400 less
  #       the log's height (B1 + 1), the last two the same; flood lines.
  #   every note: its rows are where B says. From "flood 1" on, each line
  #       on serial is one row of the log (notes and test.sh's lines
  #       aside), so a row's place is its line's place there; the newest
  #       row is 1 row old, the bottom row B, the top row B + B1.
  local why
  why=$(awk '
      function text(s) { s = substr(s, 1, 56); sub(/ +$/, "", s); return s }
      /^\[console note\] view / {
        s = substr($0, 21)
        i = index(s, " rows back of ");        b[++n] = substr(s, 1, i - 1) + 0; s = substr(s, i + 14)
        i = index(s, " kept, top row: ");      k[n] = substr(s, 1, i - 1) + 0;   s = substr(s, i + 16)
        i = index(s, ", bottom row: ");        t[n] = substr(s, 1, i - 1);       s = substr(s, i + 14)
        i = index(s, ", newest row: ");        u[n] = substr(s, 1, i - 1);       w[n] = substr(s, i + 14)
        next
      }
      /^\[console note\]|^\[test\.sh\]/ { next }
      $0 == "flood 1" && !row { row = 1 }
      row { line = text($0); if (!(line in at)) at[line] = row; row++ }
      END {
        if (n != 20) { print "expected 20 notes, saw " n; exit }
        if (b[3] <= b[1] || t[3] != t[1]) { print "new lines while scrolled back: first " b[1] " back, top \"" t[1] "\"; then " b[3] ", top \"" t[3] "\""; exit }
        if (k[20] != 400) { print "kept " k[20] " rows, not 400"; exit }
        if (b[20] != 400 - (b[1] + 1) || b[19] != b[20]) { print "oldest row: " b[19] ", " b[20] " back, not " 400 - (b[1] + 1); exit }
        if (t[20] !~ /^flood [0-9]+$/ || u[20] !~ /^flood [0-9]+$/) { print "top and bottom at the oldest: \"" t[20] "\", \"" u[20] "\""; exit }
        for (i = 1; i <= n; i++) {
          if (!(w[i] in at)) { print "note " i ": newest row \"" w[i] "\" is not a line on serial"; exit }
          newest = at[w[i]] + 1                # the row in progress: 0 rows old
          if (!(t[i] in at) || at[t[i]] != newest - b[i] - b[1]) { print "note " i ": " b[i] " back from \"" w[i] "\", but the top row is \"" t[i] "\""; exit }
          if (b[i] > 0 && (!(u[i] in at) || at[u[i]] != newest - b[i])) { print "note " i ": " b[i] " back from \"" w[i] "\", but the bottom row is \"" u[i] "\""; exit }
        }
        print "ok " b[1] " " b[3] " " b[20] " " t[20] " to " u[20] ", every row in place"
      }' "$1")
  case $why in
    ok*) echo "[test.sh] flood checked: ${why#ok }" >> "$1" ;;
    *)   echo "[test.sh] flood wrong: ${why:-no notes}" >> "$1" ;;
  esac
}

check_scroll() { # check_scroll <serial log>: the scroll case's key presses
  # PageDown at the newest line stays at 0; PageUp twice is one page, then
  # two; PageDown is one again; sixteen more PageUps stop at the oldest row
  # kept (each step a page or less, the last ones equal); Enter on the
  # typed "hi" goes back to the newest line before printing it.
  # (Only notes after the keyboard driver started: a self-test scrolls too.)
  local views
  views=$(sed -n '/keyboard ready/,$ s/^\[console note\] view \([0-9]*\) rows back.*/\1/p' "$1" | tr '\n' ' ')
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
  # "vbe-lies": a stage2 that believes a lying video BIOS (Entry 20).
  [ "$demo" = vbe-lies ] && build=(FREYA_DEMO= TEST_VBE_LIES=1)
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
  [ "$name" = log-flood ] && check_flood "$dir/serial.log"
  [ "$name" = mouse ] && check_mouse "$dir/serial.log"
  [ "$name" = shell ] && check_shell "$dir/serial.log"
  [ "$name" = programs ] && check_programs "$dir/serial.log"
  [ "$name" = revoke ] && check_revoke "$dir/serial.log"
  [ "$name" = heap ] && check_heap "$dir/serial.log"
  # After the console driver is killed: init starts it again (kill-console),
  # or, with init gone too, nothing does (kill-init).
  after=$(sed -n '/console driver killed/,$ p' "$dir/serial.log" | grep '^\[console note\] ')
  if [ "$name" = kill-console ] && echo "$after" | grep -q '^\[console note\] started' \
      && echo "$after" | sed -n '/started/,$ p' | grep -q '^\[console note\] view [1-9]'; then
    echo "[test.sh] started again: it draws and scrolls" >> "$dir/serial.log"
  fi
  # The first line-being-typed note from the new console driver is the
  # bare prompt: init told the shell, which sent it before any key.
  if [ "$name" = kill-console ] && [ "$(echo "$after" | sed -n '/started/,$ p' | grep -m1 '^\[console note\] input row: ')" = "[console note] input row: freya> " ]; then
    echo "[test.sh] started again: the shell sent it the line being typed" >> "$dir/serial.log"
  fi
  # The shell killed (Entry 23): the one init started again answers.
  if [ "$name" = kill-shell ]; then
    new=$(sed -n "s/.*the shell (process [0-9]*) ended: started it again as process \([0-9]*\) .*/\1/p" "$dir/serial.log" | head -1)
    if [ -n "$new" ] && grep -q "^\[user $new 'shell'\] hi$" "$dir/serial.log"; then
      echo "[test.sh] the shell started again answered" >> "$dir/serial.log"
    fi
  fi
  if [ "$name" = kill-init ] && [ -z "$after" ]; then
    echo "[test.sh] no console notes after it ended" >> "$dir/serial.log"
  fi
  if [ "$name" = kill-console-often ] && grep -q 'stays stopped' "$dir/serial.log" \
      && [ -z "$(sed -n '/stays stopped/,$ p' "$dir/serial.log" | grep '^\[console note\] ')" ]; then
    echo "[test.sh] no console notes after it stayed stopped" >> "$dir/serial.log"
  fi

  missing=()
  # Split on ";;" exactly (IFS would split on every single ";").
  mapfile -t wanted < <(printf '%s;;%s\n' "$patterns" "$COMMON" | sed 's/;;/\n/g')
  for p in "${wanted[@]}"; do
    [ -z "$p" ] && continue
    grep -qE -- "$p" "$dir/serial.log" || missing+=("$p")
  done
  # The console driver's notes quote rows of the log, so neither check
  # below looks at them (Entry 20).
  failed_lines=$(grep -vE '^\[console note\] ' "$dir/serial.log" | grep -E ': FAIL$')
  # A process's line starting in the middle of another line means two
  # writers were interleaved (Entry 17). Every case checks for it.
  interleaved=$(grep -nE '.\[user [0-9]+ ' "$dir/serial.log" | grep -vE '^[0-9]+:\[console note\] ' | head -2)

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
