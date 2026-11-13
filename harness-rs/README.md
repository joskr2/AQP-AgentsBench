# harness-rs — esqueleto de harness de agentes de código en Rust

> **Estado: NO compilado ni verificado (no hay toolchain Rust en la máquina donde se generó).**
> En el entorno de generación no existe `rustc` ni `cargo` y no se instalaron (restricción explícita).
> El código se escribió de forma conservadora (edition 2021, versiones estables de las crates), pero
> **ningún `cargo check`, `cargo test` ni ejecución ha ocurrido**. Trátalo como un esqueleto a validar
> con `cargo test` antes de confiar en él.

## Propósito

Este workspace reproduce, en miniatura y en Rust, la arquitectura de DeepSeek Harness (DSH):
un **log append-only como fuente de verdad**, un **bucle turno→paso** que no guarda el historial
en memoria, y dos **seams** claros (proveedor LLM y herramientas). No es una reimplementación de DSH:
es un banco de pruebas legible para medir y comparar harnesses (por ejemplo, contra el benchmark
Python de este repo vía el protocolo `aqp-agent-v1`, documentado en [docs/PROTOCOL.md](docs/PROTOCOL.md)
pero **aún no implementado**).

Objetivos:

- Código pequeño y legible; sin `unsafe`; sin dependencias exóticas.
- Límites de seguridad reales (workspace confinado, argv-only, sin claves en el log).
- Tests unitarios que fijen las invariantes importantes (log, historial derivado, edición atómica, `max_steps`).

## Estructura

```
harness-rs/
├── Cargo.toml                 # workspace virtual, resolver = "2"
├── .gitignore
├── README.md
├── docs/
│   └── PROTOCOL.md            # especificación aqp-agent-v1 (integración futura)
└── crates/
    ├── kernel/                # EventBus + Waterfall + ServiceRegistry
    ├── session/               # SessionEvent, SessionLog (JSONL), derive_messages
    ├── llm/                   # LlmProvider, Message, FakeProvider, OpenAiCompatible
    ├── tools/                 # Tool, BashTool, ReadTool, EditTool, ToolRegistry
    ├── agent/                 # AgentLoop turno→paso + cancelación
    └── cli/                   # binario `harn` (REPL y --once)
```

Todas las crates comparten versión `0.1.0` y se referencian por `path` desde
`[workspace.dependencies]`.

## Cómo compilar y ejecutar

Requiere un toolchain estable (instalación **no realizada** en la máquina de generación):

```bash
# instalar Rust (una sola vez, requiere red)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable

cd harness-rs
cargo check --workspace
cargo test --workspace
```

Ejecutar el REPL:

```bash
cargo run -p dshrs-cli -- --model deepseek-chat --base-url https://api.openai.com/v1
```

Un solo turno (útil para scripts y para el futuro adaptador del benchmark):

```bash
OPENAI_API_KEY=... cargo run -p dshrs-cli -- \
  --base-url https://api.openai.com/v1 \
  --model deepseek-chat \
  --session .harn/sessions/run-1.jsonl \
  --cwd /ruta/al/workspace \
  --once "Lee Cargo.toml y resume el workspace"
```

### Flags

| Flag | Descripción |
| --- | --- |
| `--model <name>` | Modelo enviado al endpoint. Default `deepseek-chat`. |
| `--base-url <url>` | Base del endpoint compatible con OpenAI. Default `https://api.openai.com/v1`. Apunta aquí al gateway del benchmark. |
| `--api-key-env <VAR>` | Nombre de la variable de entorno con la clave. Default `OPENAI_API_KEY`. |
| `--session <path>` | Log JSONL. Default `<cwd>/.harn/sessions/<timestamp>.jsonl`. |
| `--cwd <path>` | Raíz del workspace para herramientas y rutas relativas. Default directorio actual. |
| `--once "<prompt>"` | Ejecuta un turno y termina. |
| `--config <path>` | TOML de configuración. Default `<cwd>/harn.toml` si existe. |
| `--max-steps <n>` | Máximo de llamadas al proveedor por turno. Default 16. |

### Variables de entorno

- `HARN_MODEL` y `HARN_BASE_URL`: valores por defecto cuando no se pasa flag.
- La clave se lee **solo** de la variable indicada por `--api-key-env` / `api_key_env`
  (default `OPENAI_API_KEY`). Si no existe, se envía la petición sin cabecera
  `Authorization` (hay gateways locales que no la necesitan) y se avisa por stderr.
- La clave **nunca** se escribe en el session log ni en `harn.toml`.

### `harn.toml` (opcional)

```toml
model = "deepseek-chat"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "BENCH_GATEWAY_KEY"
max_steps = 24
system_prompt = "You are harn, a small coding agent."
```

Precedencia: flags > variables de entorno (`HARN_MODEL`, `HARN_BASE_URL`) > `harn.toml` > defaults.

## Formato del session log

Una línea JSON por evento, tag `type` en snake_case. Ejemplo real de una sesión:

```jsonl
{"type":"session_started","session_id":"4242-1699...","model":"deepseek-chat","cwd":"/tmp/ws"}
{"type":"user_message","text":"lee hello.txt"}
{"type":"tool_call","call_id":"call-1","name":"read","arguments":{"path":"hello.txt"}}
{"type":"tool_result","call_id":"call-1","name":"read","ok":true,"content":"     1\thello\n"}
{"type":"assistant_text","text":"El fichero contiene \"hello\"."}
{"type":"turn_finished","stop_reason":"end_turn"}
```

`derive_messages(&[SessionEvent])` proyecta ese log a `Vec<Message>`; el bucle del agente
**relee el log en cada paso** para construir el prompt. Esa es la propiedad clave copiada de DSH:
si el proceso muere, el historial se reconstruye exactamente desde disco.

## Mapa "idea de DSH → implementación aquí"

| Idea de DSH | Implementación en este esqueleto |
| --- | --- |
| El log append-only es la fuente de verdad del historial | `crates/session`: `SessionLog` (JSONL, flush por evento) + `derive_messages`. `AgentLoop::messages()` relee el log en cada paso. |
| Turno (input del usuario) y pasos (una respuesta del modelo) | `crates/agent`: `run_turn` con bucle de pasos, `max_steps`, y `TurnOutcome { text, stop_reason, steps }`. |
| Seam de proveedor LLM | `crates/llm`: trait `LlmProvider` (`async_trait`) con streaming de `LlmDelta`; `FakeProvider` determinista y `OpenAiCompatible` (SSE parseado a mano). |
| Seam de herramientas | `crates/tools`: trait `Tool` (`name`, `description`, `schema()`, `execute`) + `ToolRegistry`. |
| Pipeline de herramientas | `ToolRegistry::execute` despacha por nombre; el agente registra `ToolCall`/`ToolResult` en el log y emite eventos al bus antes/después de ejecutar. |
| Kernel de servicios y eventos (estilo Cordis) | `crates/kernel`: `EventBus` (`emit` broadcast), `Waterfall` (middlewares encadenados) y `ServiceRegistry` (trait objects por `TypeId`). |
| Cancelación cooperativa | `CancellationToken` (`tokio-util`) comprobado entre pasos, durante el stream y durante la ejecución de tools. |
| Mensajes derivados de eventos, no estado mutable | `derive_messages` es una función pura, sin IO, testeada por sí sola. |
| Separación configuración/historial | El `system_prompt` es configuración del `AgentLoop`; el log solo contiene historia de la conversación. |

## Decisiones de diseño

- **`SessionEvent` no guarda timestamps ni números de secuencia**: el orden de append es la
  secuencia. Menos campos, menos ambigüedad en los tests de roundtrip.
- **Un `ToolCall` por evento** y `derive_messages` lo convierte en un mensaje `assistant` con un
  `tool_calls[]` de un elemento. Es ligeramente redundante para varios tool calls en el mismo paso,
  pero trivial de auditar.
- **Bash sin shell**: `argv` es un array; no hay interpolación, globbing ni `&&`. El cwd debe
  resolverse dentro del workspace.
- **Confinamiento de rutas**: rechazo léxico de rutas absolutas y `..` **antes** de tocar el FS,
  seguido de `canonicalize` + `starts_with(workspace canónico)` para detectar symlinks que escapan.
- **Edición atómica**: si `old_string` aparece 0 o >1 veces, la edición falla; el contenido nuevo se
  escribe a un temporal en el mismo directorio y se hace `rename` sobre el destino.
- **Tool failures no abortan el turno**: se registran como `ToolResult { ok: false }` y el modelo
  decide cómo continuar. Los errores del proveedor o del log sí abortan.
- **`FakeProvider` con script**: los tests no dependen de red ni de tiempo; el script se consume
  turno a turno y el contador `calls()` permite verificar `max_steps`.
- **SSE sin dependencias extra**: buffer de bytes, split por `\n`, parseo JSON de cada `data:` y
  reensamblado de fragmentos de tool calls por `index`.
- **Sin `unsafe`** en todo el workspace.

## Qué se omite a propósito

- **Cordis / plugins dinámicos**: el kernel no carga plugins, no hay ciclo de vida de plugins ni
  aislamiento por plugin. Solo hay un `ServiceRegistry` y un bus tipado.
- **HMR / hot reload**: nada de recarga en caliente ni de entornos de desarrollo interactivos.
- **Patches YAML** sobre configuración: `harn.toml` es un TOML plano y estricto
  (`deny_unknown_fields`), sin sistema de parches.
- **Las ~120 paquetes de DSH**: aquí hay 6 crates pequeñas; no se replica la compartimentación real.
- **Subagentes y MCP**: fuera de alcance en esta fase (no hay delegación, ni servidores MCP, ni
  skill registry). **TODO explícito para la Fase 3** del roadmap.
- **Compactación de contexto / resumen automático del log**: no hay; el log crece sin límite.
- **Sandbox de sistema operativo**: el confinamiento es lógico (rutas/argv). No hay contenedores,
  seccomp, ni control de red.
- **Reintentos/backoff, timeouts de proveedor, presupuestos de tokens**: no implementados.
- **Protocolo `aqp-agent-v1`**: especificado en [docs/PROTOCOL.md](docs/PROTOCOL.md) pero **no
  implementado** (no hay modo `--protocol` ni adaptador JSONL en stdin/stdout).
- **Concurrencia multi-turno / multi-sesión**: `AgentLoop` asume un turno a la vez por instancia.
- **Persistencia de configuración de sesión, métricas y telemetría**: no hay.

## Roadmap

### Fase 1 — esqueleto ejecutable (este código, pendiente de compilar)

1. `cargo check`/`cargo test` limpios en las 6 crates.
2. Smoke test manual del REPL contra un endpoint local compatible con OpenAI.
3. Arreglar lo que descubra el compilador (especialmente el parseo SSE y los tipos del
   `OpenAiCompatible`/CLI; el `Waterfall` usa continuaciones boxeadas `'static` a propósito para
   evitar firmas con lifetimes de rango superior).

### Fase 2 — integración con el benchmark

1. Implementar el protocolo `aqp-agent-v1` sobre stdin/stdout con una crate `protocol` (o un
   submódulo del CLI) y un adaptador `AgentLoop` ↔ mensajes del protocolo.
2. Streaming de `message` por `TextDelta`, `input_request` desde una tool de pregunta,
   `done` con `stop_reason` traducido.
3. Apuntar `--base-url`/`--model` al gateway del benchmark y ejecutar la suite Python sin tocar
   `aqp_bench/` (el harness se mide como proceso externo).
4. Logs del harness a stderr; stdout exclusivamente protocolo.

### Fase 3 — madurez tipo DSH

1. Compactación del log (resumen de turnos antiguos manteniendo el log como verdad).
2. Registro de servicios real (cliente LLM, herramientas, sesión) y hooks/waterfall para políticas
   (aprobaciones, redacción, telemetría).
3. Subagentes como turnos aislados con su propio `SessionLog` y un `agent_id`.
4. MCP como fuente de herramientas, manteniendo el mismo trait `Tool`.
5. Presupuesto de tokens, retries/backoff y timeout por request.

## Tests incluidos (sin ejecutar todavía)

Todos usan `#[tokio::test]` donde hay async y no requieren red:

- `crates/kernel`: `emit` llega a los suscriptores; el waterfall respeta el orden y puede reescribir
  el valor; el registro rechaza duplicados.
- `crates/session`: roundtrip append/read del JSONL; log vacío; `derive_messages` mapea roles y
  descarta eventos de bookkeeping; varios turnos.
- `crates/tools`: edición con reemplazo único; rechazo de `old_string` ambiguo o ausente; rechazo de
  `..`, rutas absolutas y symlink que escapa; `atomic_write`; bash argv-only (sin expansión de shell);
  lectura por ventana de líneas; rechazo de cwd fuera del workspace.
- `crates/llm`: `FakeProvider` reproduce el script en orden y falla al agotarse; el body OpenAI
  conserva `tool_calls`/`tool_call_id`.
- `crates/agent`: un turno con una tool call `read` y luego mensaje final, verificando la secuencia
  exacta de eventos del log, el texto final y que no se supere `max_steps`; el bus publica
  `ToolCallRequested`/`TurnFinished`.

## Seguridad (modelo de amenaza mínimo)

- Las rutas de tools se validan dos veces: léxicamente y por canonicalización contra la raíz.
- `bash` no interpreta una shell: no hay inyección de comandos vía argumentos.
- Timeout por comando (default 30 s) y `kill_on_drop`.
- La API key no se registra en ningún evento del log; el log solo guarda modelo y cwd.
- No es un sandbox real: un proceso ejecutado con `bash` todavía puede acceder a red, leer
  variables de entorno o escapar del workspace mediante el propio binario. Para ejecución no
  confiable hace falta aislamiento del sistema operativo.

## Estado: NO compilado ni verificado (no hay toolchain Rust en la máquina donde se generó)

**NO compilado ni verificado (no hay toolchain Rust en la máquina donde se generó).** Las versiones
de dependencias se fijaron a majors estables (tokio 1, serde 1, clap 4, reqwest 0.12, async-trait 0.1,
thiserror 1, futures-util 0.3, tracing 0.1, toml 0.8, tokio-util 0.7) y el código evita características
exóticas, pero pueden existir errores de compilación o de tipos. El primer paso en una máquina con
Rust debe ser `cargo test --workspace` y corregir lo que aparezca.
