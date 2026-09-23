#!/bin/sh
# Prints an ordinary, decidedly non-JSON log line and exits with the code given
# as the first argument (0 when none is given).
printf 'running tests... 3 passed, 1 failed\n'
exit "${1:-0}"
