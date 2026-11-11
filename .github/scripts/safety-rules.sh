#!/usr/bin/env bash
# The safety rules in CLAUDE.md that a machine can check. Run from anywhere:
#   bash .github/scripts/safety-rules.sh
# Each check says what it enforces; any finding fails the run. A check that needs to change
# goes with a change to the rule it enforces.
set -u
cd "$(dirname "$0")/../.." || exit 2
SRC=rust/src
fail=0

# rule, what is enforced, offending lines (empty = ok)
check() {
    if [ -n "$3" ]; then
        echo "::error::Safety rule $1: $2"
        printf '%s\n' "$3" | sed 's/^/    /'
        fail=1
    else
        echo "ok   rule $1: $2"
    fi
}

# grep -rn output minus lines that are only a comment: a rule may be named in prose.
code_only() { grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true; }

# Rule 1: I2C is read-only. NVAPI's I2C write (NvAPI_I2CWriteEx, interface 0x283AC65A) is
# never bound, and no SMBus write transfer is built.
check 1 "no I2C write is bound or built (NvAPI_I2CWriteEx, I2C_SMBUS_WRITE)" \
    "$(grep -rniE '0x283ac65a|I2CWriteEx|I2C_SMBUS_WRITE' "$SRC" | code_only)"

# Rule 2: the service makes no network connections. Its build (no default features) has no
# HTTP/TLS crate at all, and network code lives only in the UI.
if tree=$(cd rust && cargo tree --locked --no-default-features -e normal --prefix none --target all); then
    check 2 "the service-only build contains no network or TLS crate" \
        "$(printf '%s\n' "$tree" | grep -E '^(ureq|native-tls|reqwest|hyper|openssl|rustls|curl) ' | sort -u)"
else
    echo "::error::cargo tree failed: rule 2 not checked"
    fail=1
fi
check 2 "network code lives only under src/ui/" \
    "$(grep -rnE 'ureq|native_tls|TcpStream|UdpSocket' "$SRC" | grep -v "^$SRC/ui/" | code_only)"

# Rule 4: shutdown goes through absolute paths, never PATH.
check 4 "shutdown commands are started by absolute path" \
    "$(grep -rnE 'Command::new\("(shutdown|shutdown\.exe|systemctl|poweroff)"\)' "$SRC" | code_only)"

# Rule 9: NVAPI's V/F curve write is the overclock's only.
check 9 "the V/F curve write is called only from src/overclock/" \
    "$(grep -rnE 'write_deltas\(' "$SRC" | grep -vE "^$SRC/(nvapi\.rs|overclock/)" | code_only)"

# Rule 10: the XBAR / SYS channel carries only the commands its checks cover. Adding one
# means extending the checks in clkctl.rs and this list together.
allowed=" CMD_GET_INFO CMD_GET_CONTROL CMD_SET_CONTROL CMD_MEASURE CMD_GET_ATTACHED_IDS CMD_GET_ID_INFO_V2 CMD_BUS_GET_PCI_INFO "
unknown=""
for c in $(grep -rhoE 'const CMD_[A-Z0-9_]+' "$SRC/overclock/clkctl.rs" "$SRC/overclock/clkctl" | sed 's/const //' | sort -u); do
    case "$allowed" in *" $c "*) ;; *) unknown="$unknown$c"$'\n' ;; esac
done
check 10 "the clock-domain channel sends only the checked commands" "${unknown%$'\n'}"
check 10 "SET_CONTROL is sent only from clkctl.rs, behind the write check" \
    "$(grep -rnE 'CMD_SET_CONTROL' "$SRC" | grep -vE "^$SRC/overclock/clkctl\.rs:" | code_only)"

exit $fail
