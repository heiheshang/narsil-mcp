#!/usr/bin/env bash
# Профиль индексации под samply: profile-index.sh <out.json.gz> <rate> <бинарь> [аргументы...]
#
# Индексация идёт в фоновой задаче, а сервер выходит по EOF stdin, поэтому
# с </dev/null он закрылся бы до индексации. stdin держится открытым, пока в
# stderr не появится строка о конце индексации.

set -euo pipefail

out=$1
rate=$2
shift 2

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkfifo "$work/stdin"
log=$work/stderr.log
: >"$log"

echo "profile-index: binary=$1" >&2
echo "profile-index: args=${*:2}" >&2
echo "profile-index: rate=${rate} Hz, output=${out}" >&2

started=$(date +%s)
samply record --save-only --rate "$rate" -o "$out" -- "$@" \
    <"$work/stdin" 2> >(tee -a "$log" >&2) &
pid=$!
exec 3>"$work/stdin"

done_re='Re-indexing complete|Error during re-indexing|Background initialization complete|Error during background initialization'
until grep -qE "$done_re" "$log"; do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "profile-index: сервер вышел до конца индексации" >&2
        break
    fi
    sleep 0.2
done
echo "profile-index: индексация закончилась за $(($(date +%s) - started)) с, закрываю stdin" >&2
exec 3>&-

wait "$pid"
