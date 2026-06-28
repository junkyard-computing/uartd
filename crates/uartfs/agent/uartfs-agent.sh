#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# uartfs phone-side agent. A dependency-light receiver for the uartfs protocol, speaking over
# the serial console (stdin = host->device frames, stdout = device->host replies). Needs only
# a POSIX shell + coreutils/busybox basics: base64, sha256sum, dd, wc, tr, cat, mkdir, rm.
#
# Protocol (one frame per line):
#   host->device:  UFS> PING | OPEN xid n chunksize sha | DATA xid seq b64 sum | CLOSE xid
#                  UFS> EXEC cid b64cmd
#   device->host:  UFS< READY v | ACK xid seq | NAK xid seq | DONE xid ok|fail sha
#                  UFS< OUT cid stream seq b64 | EXIT cid code out_frames out_sha
#
# A transfer reconstructs a verified blob at $UARTFS_DIR/<xid>/out; apply actions (dd, insmod,
# decompress, bspatch) are then ordinary EXEC commands that reference that path.

set -f                                   # no globbing of frame tokens
BASE="${UARTFS_DIR:-/tmp/uartfs}"
WRAP=512                                  # OUT payload width (chars)
mkdir -p "$BASE" 2>/dev/null
stty -echo 2>/dev/null                    # don't echo host input back onto the line

# cksum <body...> : first 8 hex of sha256 over the body text (the frame's KIND + args,
# single-space-joined), matching the host's frame_cksum().
cksum() { printf '%s' "$*" | sha256sum | cut -c1-8; }

# send <body...> : emit one reply frame with a trailing per-frame checksum token.
send() { _b="$*"; printf 'UFS< %s %s\n' "$_b" "$(cksum "$_b")"; }

# emit_stream <file> <stream> <cid> : print OUT frames to the console, set LAST_NB to the
# frame count. (Must NOT be called via command substitution — that would swallow the frames.)
emit_stream() {
    _f="$1"; _stream="$2"; _cid="$3"
    base64 "$_f" 2>/dev/null | tr -d '\n' > "$_f.b64"
    _n=$(wc -c < "$_f.b64" | tr -d ' ')
    _nb=$(( (_n + WRAP - 1) / WRAP ))
    _s=0
    while [ "$_s" -lt "$_nb" ]; do
        _p=$(dd if="$_f.b64" bs="$WRAP" skip="$_s" count=1 2>/dev/null)
        send "OUT $_cid $_stream $_s $_p"
        _s=$(( _s + 1 ))
    done
    LAST_NB=$_nb
}

send "READY 1"

while IFS= read -r line; do
    # resync: ignore anything that isn't a host frame; strip any console prefix on the line
    case "$line" in
        *"UFS> "*) rest=${line##*"UFS> "} ;;
        *) continue ;;
    esac
    # split off the trailing checksum token, verify it over the remaining body. A garbled or
    # merged line is rejected here (resync) rather than mis-parsed as a valid frame.
    # shellcheck disable=SC2086
    set -- $rest
    [ "$#" -ge 2 ] || continue          # need at least KIND + CKSUM
    eval "_ck=\${$#}"                    # last token = checksum
    _body=${rest%% *}                    # placeholder; rebuilt below without the cksum
    # rebuild body = all tokens except the last, single-space-joined
    _body=""; _i=1
    while [ "$_i" -lt "$#" ]; do
        eval "_t=\${$_i}"
        if [ -z "$_body" ]; then _body=$_t; else _body="$_body $_t"; fi
        _i=$(( _i + 1 ))
    done
    [ "$(cksum "$_body")" = "$_ck" ] || continue   # checksum mismatch -> not a frame
    # shellcheck disable=SC2086
    set -- $_body
    kind=$1
    shift 2>/dev/null

    case "$kind" in
    PING)
        send "READY 1"
        ;;
    QUIT)
        send "BYE"
        break
        ;;
    OPEN)
        xid=$1; nchunks=$2; sha=$4
        d="$BASE/$xid"
        rm -rf "$d" 2>/dev/null
        mkdir -p "$d" 2>/dev/null
        printf '%s' "$nchunks" > "$d/.n"
        printf '%s' "$sha" > "$d/.sha"
        ;;
    DATA)
        xid=$1; seq=$2; b64=$3; sum=$4
        d="$BASE/$xid"
        got=$(printf '%s' "$b64" | sha256sum | cut -c1-16)
        if [ "$got" = "$sum" ] && printf '%s' "$b64" | base64 -d > "$d/c.$seq" 2>/dev/null; then
            send "ACK $xid $seq"
        else
            send "NAK $xid $seq"
        fi
        ;;
    CLOSE)
        xid=$1
        d="$BASE/$xid"
        n=$(cat "$d/.n" 2>/dev/null)
        want=$(cat "$d/.sha" 2>/dev/null)
        : > "$d/out"
        seq=0; okcat=1
        while [ "$seq" -lt "${n:-0}" ]; do
            if [ -f "$d/c.$seq" ]; then
                cat "$d/c.$seq" >> "$d/out"
            else
                okcat=0; break
            fi
            seq=$(( seq + 1 ))
        done
        got=$(sha256sum "$d/out" 2>/dev/null | cut -c1-64)
        if [ "$okcat" = 1 ] && [ "$got" = "$want" ]; then
            send "DONE $xid ok $got"
        else
            send "DONE $xid fail -"
        fi
        ;;
    EXEC)
        cid=$1; b64cmd=$2
        cmd=$(printf '%s' "$b64cmd" | base64 -d 2>/dev/null)
        t="$BASE/exec.$cid"
        mkdir -p "$t" 2>/dev/null
        sh -c "$cmd" > "$t/o" 2> "$t/e"
        code=$?
        emit_stream "$t/o" 1 "$cid"; onb=$LAST_NB
        emit_stream "$t/e" 2 "$cid"
        osha=$(sha256sum "$t/o" 2>/dev/null | cut -c1-64)
        rm -rf "$t" 2>/dev/null
        send "EXIT $cid $code $onb $osha"
        ;;
    *)
        : # unknown frame; ignore
        ;;
    esac
done
