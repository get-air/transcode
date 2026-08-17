# Security

## Reporting

Please report vulnerabilities privately to the maintainers rather than opening a public issue with source credentials or private media URLs.

## Deployment model

The server binds to loopback by default. Treat binding it to a LAN or public interface as an explicit security decision and place authentication in front of it.

Source URLs and header values may contain credentials. They are retained only in memory for the session, are not returned by the API, are not included in HLS URLs, and must not be logged. Session IDs are opaque but are not an authentication mechanism.

The service is intentionally capable of accessing private HTTP origins for local media use. A remotely exposed deployment must enforce its own source allowlist to prevent SSRF.

Generated cache files are recoverable and deleted with their session. They may contain decoded media content and should live on storage with appropriate operating-system permissions.

