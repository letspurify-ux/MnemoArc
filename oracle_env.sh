#!/bin/zsh

export ORACLE_INSTANTCLIENT="/Users/iceblue/Downloads/instantclient_23_26"
export PATH="$ORACLE_INSTANTCLIENT:$PATH"
export DYLD_LIBRARY_PATH="$ORACLE_INSTANTCLIENT${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
export TNS_ADMIN="$ORACLE_INSTANTCLIENT/network/admin"

echo "Oracle Instant Client environment loaded:"
echo "  ORACLE_INSTANTCLIENT=$ORACLE_INSTANTCLIENT"
echo "  TNS_ADMIN=$TNS_ADMIN"
