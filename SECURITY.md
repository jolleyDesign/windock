# Security policy

## Supported versions

Windock hasn't had a release yet. Security fixes go into `main`; once releases start, the latest
release will be supported as well.

## Reporting a vulnerability

Please don't report security issues in public issues, discussions or pull requests.

Report them privately through GitHub instead: on the repository's **Security** tab, choose
**Report a vulnerability**. Include what an attacker could do, the steps or a proof of concept that
show it, and the Windows and Windock versions you used.

You'll get a reply within a week. Once there's a fix, it ships with an advisory crediting you,
unless you'd rather not be named.

## Scope

Windock runs as the signed-in user with no elevated rights, so the main concerns are:

- Ways another program or a crafted file (a config, shortcut or dropped file) could get Windock to
  run code or open something the user didn't choose.
- Messages from other apps, such as tray icon updates, that make Windock crash or misbehave.
- Leaking data it can see, like window titles or thumbnails, to other users or processes.
