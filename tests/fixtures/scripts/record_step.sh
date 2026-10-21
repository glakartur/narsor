#!/bin/sh
# Leaves a file named after the step (the first argument) in the working
# directory, then reports every step file the directory holds as the variable of
# that name. The value only comes out right when every earlier step's file is
# still there, which is what makes it a witness of the workspace surviving.
printf '%s\n' "$1" > "$1.step"
seen=$(ls ./*.step | sed 's|^\./||; s|\.step$||' | sort | tr '\n' ' ' | sed 's/ $//')
printf '{"choice": "done", "variables": {"%s": "%s"}}\n' "$1" "$seen"
