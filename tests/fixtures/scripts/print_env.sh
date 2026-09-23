#!/bin/sh
# Prints the values of the environment variables named as arguments, one per
# line, in the order they were named.
for name in "$@"; do
    eval "printf '%s\n' \"\$$name\""
done
