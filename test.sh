#!/usr/bin/env bash

wasm-pack build --target web &&	cp test/index.html pkg &&	node test/server_test.js
