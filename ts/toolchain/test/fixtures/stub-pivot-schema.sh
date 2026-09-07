#!/bin/sh
# A stand-in for the `pivot-schema` binary. Only the exits and the stream
# discipline matter here; the real contracts are pinned in @sip/contracts.
case "$1" in
  schema)
    printf '{"title":"%s"}\n' "$2"
    exit 0
    ;;
  fmt)
    case "$3" in
      *canonical.json) exit 0 ;;
      *unsorted.json)
        printf 'pivot-schema: %s: not canonically formatted\n' "$3" >&2
        exit 1
        ;;
      *)
        printf 'pivot-schema: %s: expected value at line 1 column 1\n' "$3" >&2
        exit 1
        ;;
    esac
    ;;
  lint)
    case "$3" in
      *clean.json)
        printf '{\n  "diagnostics": []\n}\n'
        exit 0
        ;;
      *broken.json)
        # The load-bearing case: exit 1 WITH a valid report on stdout.
        printf '{\n  "diagnostics": [\n    {\n      "hint": "declare it in `legs`",\n      "message": "leg \\"C\\" is not declared",\n      "path": "flow[id:s3]",\n      "rule": "references/leg-undeclared",\n      "severity": "error"\n    }\n  ]\n}\n'
        printf 'pivot-schema: %s: lint failed\n' "$3" >&2
        exit 1
        ;;
      *)
        printf 'pivot-schema: %s: No such file or directory\n' "$3" >&2
        exit 1
        ;;
    esac
    ;;
esac
printf 'pivot-schema: unknown command %s\n' "$1" >&2
exit 1
