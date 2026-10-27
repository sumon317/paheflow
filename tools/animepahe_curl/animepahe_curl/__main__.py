"""Allow ``python -m animepahe_curl`` during tests and local development."""

from .cli import main

raise SystemExit(main())
