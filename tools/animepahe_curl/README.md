# animepahe-curl

A tiny `curl`-compatible command-line client used by AnimePahe GUI when the
real [`curl-impersonate`](https://github.com/lwthiker/curl-impersonate) binary
is not installed. It supports only the options the GUI and the vendored
`animepahe-dl.sh` backend use.

```text
animepahe-curl [-sS] [-L] [--compressed] [--connect-timeout SECONDS]
               [--max-time SECONDS] [-b COOKIE] [-A USER_AGENT]
               [-H 'Name: value'] [-X METHOD] [--data-raw DATA] URL
```

The request uses `curl_cffi` with Chrome TLS/HTTP impersonation, which is what
gets past the Cloudflare check on `animepahe.pw`.

This package is installed into the project's `.uv/bin` directory by
`requirements.sh`. It is not a general replacement for curl.
