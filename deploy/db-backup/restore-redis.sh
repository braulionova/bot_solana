#!/bin/bash
# Restore Redis pool metadata from backup
# Usage: ./restore-redis.sh [redis-url]
REDIS=${1:-redis://127.0.0.1/}
python3 -c "
import redis, json
r = redis.from_url('$REDIS')
with open('redis-meta.json') as f:
    data = json.load(f)
pipe = r.pipeline()
for key, val in data.items():
    pipe.set(key, json.dumps(val), ex=86400)
pipe.execute()
print(f'Restored {len(data)} pool metadata keys to Redis')
"
