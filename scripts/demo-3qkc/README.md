# demo-3qkc

Despliegue local de **3 QKC en serie + 2 quditto compartidos** para
medir throughput y latencia end-to-end sin necesidad de ORR ni de SDN.

## Topología

```
   sender (test-client)                                    listener (test-client)
        │                                                        ▲
        ▼                                                        │
   QKC-1  ←──  quditto-A:8081  ──→  QKC-2  ←──  quditto-B:8082  ──→  QKC-3
   :7001                              :7002                          :7003   ← peer (QKC↔QKC)
   :7101                              :7102                          :7103   ← local (ORR↔QKC)
   :7201                              :7202                          :7203   ← admin HTTP
```

- El cliente de prueba (binario `qkc-test-client`) abre el `local_listen`
  de **QKC-1** y manda frames `LOCAL_SEND` con `dest_final = 3`.
- QKC-1 cifra con `quditto-A`, manda `FRAME_RELAY` a QKC-2.
- QKC-2 descifra con `quditto-A`, recifra con `quditto-B`, manda
  `FRAME_RECV` a QKC-3 (next-hop == dest_final).
- QKC-3 descifra con `quditto-B` y entrega `LOCAL_DELIVER` al cliente
  conectado a su `local_listen`.

## Uso

```bash
cd scripts/demo-3qkc
chmod +x *.sh                     # primera vez

./start.sh                        # compila + arranca todo + popula forwarding tables
./send-one.sh                     # un mensaje de prueba end-to-end
./stress.sh 10000 64              # 10000 frames de 64 B
./stress.sh 1000 4096             # 1000 frames de 4 KiB
./stop.sh                         # apaga todo
```

Logs: `/tmp/dkms-rust-demo/*.log` (uno por proceso).
PIDs: `/tmp/dkms-rust-demo/pids`.

## Qué mirar para detectar cuellos

`stress.sh` reporta dos números:

- **send-side throughput**: cuánto puede absorber QKC-1 en la cola
  de salida (sin esperar deliveries). Si esto baja, el cuello está en
  el encrypt + cola lock-free del peer-client.
- **end-to-end throughput**: cuántos frames terminan entregados en
  QKC-3 por segundo. Si esto es mucho menor que send-side, el cuello
  está en algún hop intermedio: descifrar+recifrar de QKC-2, o
  decrypt de QKC-3, o el quditto consumiendo claves más rápido de lo
  que las genera.

Para validar manualmente cada parte:

```bash
# Healthz de cada QKC
for p in 7201 7202 7203; do curl -s http://127.0.0.1:$p/healthz; echo; done

# Snapshot de la forwarding table
curl -s http://127.0.0.1:7201/forwarding-table

# Status del quditto A (cuántas claves frescas tiene)
curl -s http://127.0.0.1:8081/api/v1/keys/qkc/status

# Buffer del quditto B
curl -s http://127.0.0.1:8082/api/v1/keys/qkc/status
```

Si el QKC-1 satura a quditto-A pidiendo claves más rápido de lo que
genera, ese es un cuello esperado: súbele el `--r0` en `start.sh`.

## Limpiar

```bash
./stop.sh
rm -rf /tmp/dkms-rust-demo
```
