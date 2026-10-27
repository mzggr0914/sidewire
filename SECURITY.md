# Security

[한국어](SECURITY.ko.md)

SideWire can expose root-capable Android operations over the network, so security bugs should not be filed as public issues before a fix is available.

## Supported versions

Security fixes are provided for the latest stable release only. Older releases and prereleases are not maintained for security fixes. Please use the [latest stable release](https://github.com/mzggr0914/sidewire/releases/latest).

## Reporting a vulnerability

Use GitHub's private vulnerability reporting when it is available for the repository.

Please include:

- SideWire version
- Android version / ROM and KernelSU version
- host OS
- connection mode (`inbound` or `outbound`)
- whether the session was `secure` or `insecure`
- reproduction steps and expected impact

Avoid including real pairing secrets, trust-store contents, or other credentials in reports.

## Network exposure

Secure mode is the default. Insecure mode disables authentication and encryption and should only be used on a network you fully trust.

SideWire is designed for trusted LAN/VPN use. Do not expose its listening ports directly to the public Internet.
