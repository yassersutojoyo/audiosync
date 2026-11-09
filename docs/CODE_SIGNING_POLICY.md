# Code signing policy

> **Placeholder:** Windows releases aren't signed yet. I'm applying to the SignPath Foundation's free signing program for open source projects. Once the project is accepted, this note goes and the line below applies.

Free code signing provided by [SignPath.io](https://signpath.io), certificate by [SignPath Foundation](https://signpath.org).

## What gets signed

- `audiosync.exe`, the Windows app, for x64 and ARM64, as it ships in the release zips
- `AudioSync-Setup.exe`, the Windows installer, once it's part of a release

Nothing else: only files built from this repository's own code.

## How a signed build is made

Every Windows build comes from GitHub Actions running on this public repository, never from my own PC. Each build records the commit it was built from in `SOURCE-COMMIT.txt`, and the release steps check that it's the release's commit.

A build is signed only when it's a release I've approved. I approve each signing request by hand.

## Team roles

AudioSync has one maintainer, so I hold all three roles:

- Authors (may change the code without another review): [yassersutojoyo](https://github.com/yassersutojoyo)
- Reviewers (review every change from anyone else before it's merged): [yassersutojoyo](https://github.com/yassersutojoyo)
- Approvers (approve every signing request): [yassersutojoyo](https://github.com/yassersutojoyo)

## Privacy

Besides the speakers you pick and the Web Remote on your own network, AudioSync makes two requests on its own: the update check to GitHub, which you can turn off, and on the Mac, Spotify's cover art while Spotify plays. [PRIVACY.md](../PRIVACY.md) has the details.
