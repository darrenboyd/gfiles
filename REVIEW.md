# Review policy

@CODING_STANDARDS.md

This file holds how code review behaves in gfiles; the rules the code follows are in
`CODING_STANDARDS.md`, included above.

## What matters most

gfiles runs on every machine Darren owns and holds the map of all his files, so two areas
outrank everything else:

- **Security from outside:** anything a remote machine, an AI client over MCP, or other
  untrusted input could abuse, such as the export/pull SSH boundary, tool arguments, SQL and
  FTS, and paths.
- **Safety with files:** anything that could modify, delete or escape out of user files, or
  make the catalog wrong about what exists (which later move, rename and delete actions
  would act on).

Rank findings in these areas above other findings of similar likelihood, and don't drop
them for being unlikely; say how likely they are instead.
