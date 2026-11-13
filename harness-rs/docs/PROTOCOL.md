# Protocolo `aqp-agent-v1`

Especificación del protocolo de línea JSON (JSONL) que permitirá medir este harness con el
benchmark Python del repositorio (`aqp_bench/`) como un **proceso externo**.

> **Estado de implementación: NO IMPLEMENTADO.** Este documento es solo la especificación para una
> integración futura. El binario `harn` actual no habla este protocolo: usa un REPL por stdin y
> `--once`. Ver el roadmap del [README](../README.md).

## Especificación

- Una línea = un objeto JSON UTF-8 + `\n` por stdin/stdout.
- Harness → agente: `{"type":"prompt","id":"...","model":"...","prompt":"..."}`;
  `{"type":"answer","request_id":"...","value":<JSON>}`; `{"type":"stop"}`.
- Agente → harness: `{"type":"agent_start","agent_id":"main"}`;
  `{"type":"agent_end","agent_id":"main"}`; `{"type":"message","text":"..."}`;
  `{"type":"input_request","request_id":"...","question":"...","metadata":{}}`;
  terminal `{"type":"done","stop_reason":"end_turn"}` o
  `{"type":"done","stop_reason":"error","error":"..."}`.
  EOF antes de `done` es error; aceptar el prompt no significa terminar.

## Mensajes, campo por campo

### Harness → agente

| `type` | Campos | Significado |
| --- | --- | --- |
| `prompt` | `id` (string), `model` (string), `prompt` (string) | Pide al agente procesar un turno. `id` es opaco y lo elige el harness; se usa para correlacionar. `model` es la selección de modelo para este turno (el agente no debe asumir que es constante entre turnos). |
| `answer` | `request_id` (string), `value` (JSON) | Respuesta a un `input_request` previo del agente. `request_id` debe coincidir. |
| `stop` | (ninguno) | Petición de parada ordenada. El agente debe terminar el trabajo en curso como máximo y emitir su `done`. |

### Agente → harness

| `type` | Campos | Significado |
| --- | --- | --- |
| `agent_start` | `agent_id` (string) | El agente empezó a atender un turno. `agent_id` identifica al agente; en esta versión siempre `"main"`. |
| `agent_end` | `agent_id` (string) | El agente terminó de atender el turno (informativo; no sustituye a `done`). |
| `message` | `text` (string) | Texto visible para el usuario/harness. Puede emitirse varias veces por turno (streaming). |
| `input_request` | `request_id` (string), `question` (string), `metadata` (object) | El agente necesita información del harness para continuar. `metadata` es libre y debe ser un objeto JSON (nunca `null`). |
| `done` | `stop_reason` (string), `error` (string, solo si `stop_reason` es `"error"`) | **Mensaje terminal** del turno. `stop_reason` válido: `"end_turn"` o `"error"`. |

## Reglas de transporte

1. **JSONL estricto**: cada línea es un objeto JSON completo, sin multilínea, terminado en `\n`.
2. **UTF-8**: la codificación es UTF-8; no se admiten BOMs ni bytes inválidos.
3. **stdout es exclusivamente protocolo.** Diagnósticos, logs y warnings van a `stderr`.
4. **EOF antes de `done` es error.** Si el proceso del agente cierra stdin/stdout sin haber emitido
   el `done` del turno en curso, el harness debe tratar el turno como fallido.
5. **Aceptar el prompt no significa terminar.** El harness no puede asumir que el turno acabó al
   enviar `prompt`: debe esperar el `done` (posiblemente tras `agent_start`, `message` e
   `input_request`).
6. **Correlación**: `answer.request_id` debe coincidir exactamente con el `request_id` del
   `input_request` pendiente. Los `id` de `prompt` son opacos.
7. **Campos desconocidos**: un receptor tolerante debería ignorarlos para permitir evolución
   compatible (upgrades menores del protocolo sin romper el otro lado).
8. **Un turno a la vez**: el harness no envía un segundo `prompt` mientras el anterior no haya
   producido su `done`.

## Ejemplo de turno

Harness → agente:

```json
{"type":"prompt","id":"run-1-turn-3","model":"deepseek-chat","prompt":"¿Qué contiene README.md?"}
```

Agente → harness:

```json
{"type":"agent_start","agent_id":"main"}
{"type":"message","text":"Voy a leer el fichero."}
{"type":"message","text":"README.md describe el workspace."}
{"type":"agent_end","agent_id":"main"}
{"type":"done","stop_reason":"end_turn"}
```

Turno con interacción:

```json
{"type":"agent_start","agent_id":"main"}
{"type":"input_request","request_id":"q-9","question":"¿Sobrescribo el fichero?","metadata":{"path":"README.md"}}
{"type":"message","text":"Esperando confirmación."}
{"type":"done","stop_reason":"end_turn"}
```

(el harness responde en cualquier momento con
`{"type":"answer","request_id":"q-9","value":true}`)

Turno con error:

```json
{"type":"agent_start","agent_id":"main"}
{"type":"message","text":"Fallo del proveedor."}
{"type":"done","stop_reason":"error","error":"http 429: rate limited"}
```

## Máquina de estados (lado del agente)

```
IDLE --prompt--> RUNNING --done(stop_reason)--> IDLE
                    |
                    +--message*--> (sigue RUNNING)
                    +--input_request--> WAITING_ANSWER --answer--> RUNNING
                    +--stop--> terminando --> done
```

Notas:

- `agent_start` se emite una vez por turno, antes de cualquier `message`/`input_request`.
- `agent_end` es informativo y **no** es terminal.
- `done` es el único mensaje terminal. Después de `done` el agente queda en `IDLE` esperando otro
  `prompt`.

## Cómo se implementaría aquí (futuro)

El esqueleto actual ya tiene las piezas necesarias; la Fase 2 del roadmap las conecta:

| Pieza del protocolo | Pieza existente del harness |
| --- | --- |
| `prompt` → ejecutar un turno | `AgentLoop::run_turn(input)` (`crates/agent`). |
| `message` (streaming) | `HarnessEvent::TextDelta` en `crates/kernel`; suscripción al `EventBus`. |
| `done` (`end_turn`, `error`) | `TurnOutcome { stop_reason }`; `StopReason::Error` para fallos. |
| `stop` | `CancellationToken` pasado a `AgentLoop::with_cancellation`; el bucle termina con `StopReason::Cancelled`. |
| `input_request` / `answer` | Requiere una tool nueva ("ask user") que emita la petición y espere el `answer`; hoy **no existe**. |
| `agent_start` / `agent_end` | Se derivan de `HarnessEvent::TurnStarted` / `TurnFinished`. |
| stdout JSONL / stderr logs | El CLI actual imprime en stdout/stderr humanos; el modo protocolo escribiría solo JSON en stdout. |

Pendiente de decidir en esa fase:

- ¿El adaptador vive en el CLI (`harn --protocol aqp-agent-v1`) o en una crate `protocol` aparte?
- ¿`agent_id` se mantiene fijo (`"main"`) o se deriva de una sesión?
- ¿Los `message` se emiten por delta o agregados por paso (el protocolo permite ambos)?
- ¿Qué `metadata` estandariza `input_request` (p. ej. `path`, `options`, `default`)?

Cualquier cambio en este documento debería tratarse como un cambio de protocolo versionado
(`aqp-agent-v2` si es incompatible).
