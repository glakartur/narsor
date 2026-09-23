#!/bin/sh
# Records its own pid in the file named by the first argument and then sleeps
# far longer than any test time limit, so the test can check that the executor
# really killed the process.
printf '%s' "$$" > "$1"
while true; do
    sleep 1
done
