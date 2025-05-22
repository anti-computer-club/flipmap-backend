#!/usr/bin/env bash
# It's more accurate to think of this as an 'exploration' of the routes.

commands=(
  # Lots of things are handled implicitly by Axum and/or Hyper
  # HTTP 415 Unsupported Media Type
  'curl -v -m 30 127.0.0.1:1337/route -H "Content-Type: text/html; charset=utf-8" -d "woah dude"'
  # HTTP 405 Method Not Allowed
  'curl -v -m 30 127.0.0.1:1337/route'
  # HTTP 404 Not Found
  'curl -v -m 30 127.0.0.1:1337/wp-login.php'

  # Deserialization & Validation errors are handled higher up
  # Field has wrong type: HTTP 422 Unprocessable Entity
  'curl -v -m 30 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"src_lat\": \"Hello, is this the Krusty Krab?\",\"src_lon\": -123.277961,\"dst_lat\": 44.568638, \"dst_lon\": -123.277845}"'
  # Field fails constraint: HTTP 422 Unprocessable Entity
  'curl -v -m 30 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"src_lat\": 4444.568760,\"src_lon\": -123.277961,\"dst_lat\": 44.568638, \"dst_lon\": -123.277845}"'
  # Skipped field: HTTP 422 Unprocessable Entity (you get the idea...)
  'curl -v -m 30 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"src_lon\": -123.277961,\"dst_lat\": 44.568638, \"dst_lon\": -123.277845}"'

  # HTTP 200 Live test. Should work fine.
  'curl -v -m 30 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"src_lat\": 44.568760,\"src_lon\": -123.277961,\"dst_lat\": 44.568638, \"dst_lon\": -123.277845}"'
  # HTTP 200 Live test. Should work fine for search feature.
  'curl -v -m 30 127.0.0.1:1337/get_locations -H "Content-Type: application/json" -d  "{\"amount\": 20, \"lat\": 44.568760,\"lon\": -123.277961,\"query\":\"Starbucks\"}"'
)

for command in "${commands[@]}"; do
  # Wait for the user to press enter
  read -p "Press enter to execute: $command"
  # Execute the current curl command
  eval $command
  echo # For a newline
done
