# Security Policy

Only the latest release is supported. Please report vulnerabilities privately through GitHub's
security advisory interface rather than opening a public issue.

Build manifests are trusted project configuration, but `union-builder` deliberately does not accept
arbitrary shell commands. Repository URLs must be credential-free HTTPS GitHub URLs and all sources
must be fixed to complete commit IDs. Release builders should run with the least privileges needed
to read source trees and write a fresh output directory.

