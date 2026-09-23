#!/bin/sh
# Prints a line of 1024 'x' characters as many times as the first argument says,
# then exits 0. Used to check that captured output is capped.
count="${1:-1}"
line=$(printf '%1024s' '' | tr ' ' 'x')
i=0
while [ "$i" -lt "$count" ]; do
    printf '%s\n' "$line"
    i=$((i + 1))
done
