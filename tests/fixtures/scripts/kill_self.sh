#!/bin/sh
# Kills itself with SIGKILL, ending without any exit code at all.
printf 'about to die\n' >&2
kill -9 $$
