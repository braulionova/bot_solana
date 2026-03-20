# Helios V2 - Arquitectura y Estado Actual

## Avances Clave
- **Exact CLMM Math:** Se implementó en `pool_state.rs` la lógica matemática exacta para cotizar en las pools de liquidez concentrada de Orca. Esto resolvió el problema de "alucinaciones" que mostraban profits gigantes inalcanzables.
- **Raydium V4 Fix:** Al consultar el estado del AMM, `pool_hydrator` también obtiene y suma los saldos de los `market_vault` (de Serum/OpenBook), validando la fórmula de Constant Product ($X * Y = K$) en las pools de Raydium.
- **Dynamic ALTs:** `create_and_extend_alt` en `executor/src/alt_loader.rs` ya funciona en mainnet. Selecciona `recent_slot` desde `SlotHashes`, espera a que la ALT quede usable on-chain, y compacta transacciones grandes a tamaños simulables.
- **Pessimistic Slippage en Multi-Hops:** `tx_builder.rs` descuenta de antemano el posible slippage intra-tick al calcular el `amount_in` del siguiente salto (`safe_out = 99.5%`), erradicando los errores de `InsufficientFunds` durante la ejecución encadenada en Raydium/Orca.
- **Graph Expansion:** `mapped_pools.json` se alimenta mediante `fetch_pools.py`, conectando más de 5600 pools activas de alto volumen desde las APIs oficiales de Raydium, Orca y Meteora.
- **Meteora DLMM Integration:** Meteora DLMM ya quedó alineado end-to-end: quote exacto por bins, program ID correcto (`...Pwxo`) y whitelist on-chain de `helios-arb` actualizada en mainnet.
- **ATA Setup In-Tx:** `tx_builder.rs` ahora incluye `CreateIdempotent` para los token accounts intermedios antes del flash loan, resolviendo `AccountNotInitialized` en rutas multi-hop.
- **Telegram Alertas:** Integrado y listo para emitir notificaciones on-chain (`executor/src/telegram.rs`).

## Bloqueador Actual
El bloqueo operativo principal ya no es ALT, Meteora ni cuentas faltantes. La simulación mainnet hoy alcanza el **tercer hop Orca** de la mejor ruta, pero ese swap falla con `TickArraySequenceInvalidIndex (6038)` en el pool `7rDhNbGByYY3rbnJQDFMV9txQYZeBUNQwsMnowDRvw22`.

Hallazgos ya verificados:
- `find_routes` refresca metadata de los pools elegidos antes de construir la TX. El problema persiste, así que no parece ser solo cache vieja.
- El builder ya probó `remaining_accounts_info` real y supplemental tick arrays; con demasiados supplemental arrays Orca devuelve `6055 TooManySupplementalTickArrays`, y con ventanas más chicas vuelve a `6038`.
- El probe dirigido `executor/src/bin/probe_orca_tick_arrays.rs` ya barrió offsets y combinaciones cercanas alrededor del array actual sin encontrar un set válido para ese pool/hop.

**Próxima tarea a desarrollar:** derivar la secuencia de tick arrays desde la lógica oficial de traversal/quote de Orca para el `amount_in` real del hop, en lugar de inferirla solo desde `tick_current_index`.

## Cómo probar/correr el executor aisladamente:
`cargo run --bin find_routes`
Esto probará el bot a nivel atómico con el grafo actualizado, compilando las rutas óptimas y simulándolas.
