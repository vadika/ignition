#!/usr/bin/env bash
# run.sh — build, fuzz, and reproduce. Mirrors ignition's M0->M1 loop.
set -e
cd "$(dirname "$0")"

# ASan must turn errors into a signal so the parent's waitpid sees a "crash doorbell".
export ASAN_OPTIONS=abort_on_error=1:halt_on_error=1:detect_leaks=0

echo "==> build"
gcc -O0 -g -fsanitize=address -fsanitize-coverage=trace-pc -c target.c   -o target.o        # instrumented target
gcc -O0 -g -fsanitize=address                               -c target.c  -o target_plain.o  # uninstrumented (for repro)
gcc -O1 -g -fsanitize=address                               -c harness.c -o harness.o       # NOT coverage-instrumented
gcc -fsanitize=address target.o  harness.o -o fuzz
gcc -O0 -g -fsanitize=address repro.c target_plain.o -o repro

echo "==> fuzz (coverage-guided, fork-per-input reset)"
./fuzz "${1:-5000000}" crash.bin

echo
echo "==> reproduce the discovered crash (determinism gate)"
./repro crash.bin
