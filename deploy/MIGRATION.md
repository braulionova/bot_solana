# Migration Guide — Move Helios to New VPS

## 1. Clone repo
```bash
git clone https://github.com/braulionova/bot_solana.git
cd bot_solana
```

## 2. Build
```bash
cargo build --release
```

## 3. Restore PostgreSQL
```bash
createdb helios
cd deploy/db-backup
psql helios < schema.sql
for f in pg-full/*.sql.gz; do gunzip -c "$f" | psql helios; done
gunzip -c ml-data/ml_models.sql.gz | psql helios
```

## 4. Restore Redis
```bash
cd deploy/db-backup
./restore-redis.sh redis://127.0.0.1/
```

## 5. Configure environment
```bash
cp .env.example /etc/helios/helios.env
# Edit with your keys: wallet, Helius, Telegram, DB password
```

## 6. Copy wallet keypair (from secure backup)
```bash
# DO NOT store in git — transfer securely
scp wallet.json new-vps:/etc/helios/
scp identity.json new-vps:/etc/helios/
```

## 7. Install systemd services
```bash
cp deploy/systemd/*.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable helios-bot
systemctl start helios-bot
```

## 8. Verify
```bash
journalctl -fu helios-bot
# Should see: shreds_per_s > 1000, pool cache bootstrapped
```
