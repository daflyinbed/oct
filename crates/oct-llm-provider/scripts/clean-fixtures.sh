#!/bin/bash

for file in tests/moonshot/fixtures/generate/*.json tests/moonshot/fixtures/stream/*.json tests/anthropic/fixtures/generate/*.json tests/anthropic/fixtures/stream/*.json; do
    if [ -f "$file" ]; then
        jq '{
            request: { body: .request.body },
            response: { body: .response.body }
        }' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
        echo "Cleaned: $file"
    fi
done