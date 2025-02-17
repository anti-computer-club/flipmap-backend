#!/usr/bin/env bash
commands=(
  # Lots of things are handled implicitly by Axum and/or Hyper
  # HTTP 415 Unsupported Media Type
  'curl -v -m 30 -X POST 127.0.0.1:1337/route -H "Content-Type: text/html; charset=utf-8" -d "woah dude"'
  # HTTP 405 Method Not Allowed
  'curl -v -m 30 -X GET 127.0.0.1:1337/route'
  # HTTP 404 Not Found
  'curl -v -m 30 -X GET 127.0.0.1:1337/wp-login.php'

  # Deserialization & Validation errors are handled higher up
  # HTTP 422 Unprocessable Entity
  'curl -v -m 30 -X POST 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"lat\": \"Hello, is this the Krusty Krab?\",\"lon\": -123.277961,\"query\":\"Chum Bucket\"}"'
  # HTTP 422 Unprocessable Entity
  'curl -v -m 30 -X POST 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"lat\": -4444.568760,\"lon\": -123.277961,\"query\":\"he lat too big\"}"'
  # HTTP 422 Unprocessable Entity (you get the idea...)
  'curl -v -m 30 -X POST 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"lon\": -123.277961,\"query\":\"skipped lat day\"}"'

  # HTTP 200 Live test. Should work fine.
  'curl -v -m 30 -X POST 127.0.0.1:1337/route -H "Content-Type: application/json" -d "{\"lat\": 44.568760,\"lon\": -123.277961,\"query\":\"Downward Dog\"}"'
)

for command in "${commands[@]}"; do
  # Wait for the user to press enter
  read -p "Press enter to execute: $command"
  # Execute the current curl command
  eval $command
  echo # For a newline
done
