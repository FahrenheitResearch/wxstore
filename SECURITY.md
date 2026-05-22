# Security

WxStore is a local-first weather storage/API service. It is not an authenticated
multi-tenant edge service.

## Deployment posture

- The default bind address is `127.0.0.1`.
- Do not expose a WxStore instance directly to the public internet without a
  trusted reverse proxy, authentication, authorization, rate limiting, and
  operational monitoring.
- Treat every configured root (`--spatial-root`, `--profile-store`,
  `--static-plots-root`, `--evidence-root`, `--observations-root`, and similar)
  as data that can be surfaced through the API if the corresponding lane is
  enabled.
- Keep weather data, generated products, `.env` files, logs, and local proof
  artifacts out of git. The repository `.gitignore` covers the common cases.

## Reporting

Please report security issues privately to the repository owner before opening a
public issue. Include the affected route or command, the configured lane roots,
and a minimal reproduction when possible.
