#!/bin/sh
# Prints the §3.3.B JSON payload given as the first argument and exits with the
# code given as the second one (0 when none is given).
printf '%s\n' "$1"
exit "${2:-0}"
