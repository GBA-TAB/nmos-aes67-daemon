#!/bin/bash
#
# Called once by the daemon at startup with its configured
# interface_name(0)/interface_name(1) (primary/secondary NIC for ST 2022-7
# dual-leg operation), so lldpd's advertised port descriptions always match
# whichever physical NIC is currently primary (Red) / secondary (Blue)
# instead of a hand-maintained /etc/lldpd.d/ file drifting out of sync.
#
# Requires the daemon's user to be a member of the lldpd control socket's
# group (commonly "_lldpd") - lldpd's `configure` commands are refused
# otherwise. No sudo/root needed once that membership is set.

PRIMARY="$1"
SECONDARY="$2"

if [ -n "$PRIMARY" ]; then
  lldpcli configure ports "$PRIMARY" lldp portdescription "Primary (Red) - ST 2022-7 leg 0"
fi
if [ -n "$SECONDARY" ]; then
  lldpcli configure ports "$SECONDARY" lldp portdescription "Secondary (Blue) - ST 2022-7 leg 1"
fi
