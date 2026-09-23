#!/bin/sh
# Writes the second argument into a file named by the first one, relative to the
# working directory the process was started in.
printf '%s' "$2" > "$1"
