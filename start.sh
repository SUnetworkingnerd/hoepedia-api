#!/usr/bin/env sh
# Start Hoepedia with whatever is available on this machine:
#   1. a hoepedia-api program next to this script (a downloaded release)
#   2. Docker
#   3. Rust (builds the program the first time; takes a few minutes)
# No configuration is needed. Options after the script name are passed to the server when
# it runs directly (1 and 3), for example:  ./start.sh --port 8080
# Use  ./start.sh --native  to skip Docker.
set -eu
cd "$(dirname "$0")"

NATIVE=0
if [ "${1:-}" = "--native" ]; then NATIVE=1; shift; fi

if [ -x ./hoepedia-api ]; then
  exec ./hoepedia-api "$@"
fi

if [ "$NATIVE" = 0 ] && command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  if ! docker info >/dev/null 2>&1; then
    echo "Docker is installed but not running. Start Docker and run this again, or use: ./start.sh --native"
    exit 1
  fi
  echo "Starting Hoepedia with Docker (the first start builds the image and takes a few minutes)..."
  docker compose up -d --build
  PORT="$(grep -E '^HOST_PORT=' .env 2>/dev/null | tail -n 1 | cut -d= -f2 | tr -d '\r" ' || true)"
  PORT="${PORT:-3000}"
  i=0
  until docker compose exec -T hoepedia-api hoepedia-api --healthcheck >/dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -gt 60 ]; then
      echo "The server did not become ready. Look at: docker compose logs hoepedia-api"
      exit 1
    fi
    sleep 1
  done
  echo
  echo "  Hoepedia is running:  http://localhost:$PORT"
  echo "  Admin token:          $(docker compose exec -T hoepedia-api hoepedia-api token 2>/dev/null | tr -d '\r')"
  echo
  echo "  Test it:   docker compose exec hoepedia-api hoepedia-api check"
  echo "  Logs:      docker compose logs -f hoepedia-api"
  echo "  Stop:      docker compose down        (your data stays in the Docker volume)"
  exit 0
fi

if command -v cargo >/dev/null 2>&1; then
  if [ ! -x target/release/hoepedia-api ] || [ -n "$(find src Cargo.toml Cargo.lock -newer target/release/hoepedia-api 2>/dev/null | head -n 1)" ]; then
    echo "Building Hoepedia (the first build takes a few minutes)..."
    cargo build --release --locked
  fi
  exec ./target/release/hoepedia-api "$@"
fi

cat <<'MSG'
Hoepedia needs one of these to run:

  * Docker   https://docs.docker.com/get-docker/      (easiest)
  * Rust     https://rustup.rs                        (builds it on this machine;
             also needs a C compiler: on Debian/Ubuntu  sudo apt install build-essential)

Install one of them and run ./start.sh again.
MSG
exit 1
