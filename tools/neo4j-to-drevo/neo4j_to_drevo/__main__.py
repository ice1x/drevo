"""`python -m neo4j_to_drevo` entry point."""

from __future__ import annotations

import sys

from .cli import main

if __name__ == "__main__":
    sys.exit(main())
