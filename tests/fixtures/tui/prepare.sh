#!/bin/sh
# Stands in for the work a real `prepare` state would do: reports success and
# hands the ticket it was given back as a context variable, so the TUI's details
# panel has something to show.
printf '{"choice": "prepared", "variables": {"ticket": "%s"}}\n' "$1"
exit 0
