#!/bin/sh
# Sleeps for the number of seconds given as the first argument and then prints
# the §3.3.B JSON payload given as the second one. Long enough for a test to
# interact with an action while it is still running, short enough not to slow
# the suite down.
sleep "$1"
printf '%s\n' "$2"
