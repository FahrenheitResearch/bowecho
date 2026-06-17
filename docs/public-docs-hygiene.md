# Public Docs Hygiene

Use this checklist before publishing docs or release notes.

- No personal machine paths such as Windows, macOS, or Linux user-home paths,
  desktop paths, temporary extraction paths, local UUID session paths, or
  assistant tool-output folders.
- No actual API keys, tokens, private keys, passwords, certificate payloads,
  one-time-password seeds, app-specific passwords, or repository secret values.
- No private poll URLs, SpotterNetwork position feeds, personal placefile URLs,
  local config dumps, or cached diagnostic bundles.
- Repo-relative source paths are fine, for example
  `crates/app_ui/src/main.rs`.
- Public DOIs, paper names, package names, official URLs, public reference
  implementation filenames, and crate/module names are fine.
- Prompt excerpts are okay only when curated, intentional, and free of
  personal or private details.
- If a local artifact is useful only as verification scaffolding, describe it
  generically, such as "local research cache" or "local release test build."
- Removing a string from the latest docs does not remove it from Git history.
  If a private value ever lands in history, handle that as a separate
  coordinated history rewrite with tools such as `git filter-repo` or BFG.
