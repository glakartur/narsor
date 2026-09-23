#!/bin/sh
# Prints every argument on its own line, so a test can tell arguments that were
# passed separately from arguments that were joined into one string.
for argument in "$@"; do
    printf '%s\n' "$argument"
done
