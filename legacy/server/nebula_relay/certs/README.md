# Nebula relay TLS certificate (dev)

The relay needs a TLS cert/key. For development, generate a self-signed pair
here (these files are git-ignored and must NOT be committed):

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout relay_key.pem -out relay_cert.pem \
  -days 3650 -subj "/CN=nebula-relay"
```

The client trusts this self-signed cert (dev-only trust-all in
`platform/mac/src/RelayTransport.mm`). For production, use a real certificate
(e.g. Let's Encrypt) and remove the trust-all verify block. See `../DEPLOY.md`.
