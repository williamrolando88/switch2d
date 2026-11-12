# Reporte de Evaluación de la Guía de Instalación (End-User Run) - switch2d

Este informe documenta la evaluación estricta, paso a paso, de la guía de instalación y uso provista en [README.md](file:///var/home/wmorales/repos/switch2d/README.md) de `switch2d`, bajo una perspectiva pura de usuario final y sin realizar reparaciones proactivas en el código o en la configuración.

---

## 1. Información del Entorno de Evaluación

- **Fecha:** 2026-11-12
- **Sistema Operativo:** Linux bazzite 7.2.15-ogc1.1.fc45.x86_64 (Bazzite / Fedora Atomic Desktop)
- **Usuario:** `wmorales` (UID 1000)
- **Directorio de Trabajo:** `/var/home/wmorales/repos/switch2d`
- **Herramienta Cargo / Rust:** `cargo 1.99.1`
- **Adaptador Bluetooth:** `8C:17:59:41:D6:1E` (hci0)

---

## 2. Resultados de la Sección de Instalación (`## ⚙️ Install`)

| Paso | Instrucción en README | Resultado | Hallazgos y Observaciones de UX |
|---|---|---|---|
| **0** | `git clone ... && cd switch2d` | **PASS** | Repositorio clonado y terminal ubicada en el proyecto. |
| **1** | `cargo install --path . --locked` | **PASS** | Compiló el binario en perfil release e instaló `switch2d` (0.1.0) en `~/.cargo/bin/`. Verificado en `$PATH`. |
| **2** | `sudo install -Dm644 system/70-switch2d.rules ...` | **BLOQUEO / REQUIERE SUDO** | **1.** Requiere privilegios interactivos de `sudo`.<br>**2.** Las rutas son relativas (`system/...`); si el usuario corre el comando fuera del directorio del repositorio, falla con `cannot stat: No such file or directory`. Una vez ejecutado con elevación en el CWD correcto, instaló con éxito. |
| **3** | `sudo install ... switch2d-disable-le-privacy.service` | **REQUIERE SUDO** | Requiere elevación para `/usr/local/bin/` y `/etc/systemd/system/`. Una vez ejecutado con elevación, el servicio quedó habilitado y activo (`active (exited)`). |
| **4** | Regla sudoers para `btmgmt` | **REQUIERE SUDO** | Requiere elevación para escribir en `/etc/sudoers.d/switch2d-btmgmt`. Verificado con `sudo -n -l` (`NOPASSWD` funcional). |
| **5** | Instalación de `switch2d.service` de usuario | **PASS** | Se ejecutó en espacio de usuario. Enlace creado hacia `~/.cargo/bin/switch2d run`. |
| **6** | Verificación con `switch2d doctor` | **PASS (GATE VÁLIDO)** | El gate del README se cumplió: detectó correctamente cuando la regla de udev no estaba en el intento fallido inicial (`WARN`), y pasó a **8/8 checks en PASS** una vez aplicados los pasos anteriores. |

---

## 3. Resultados de la Sección de Emparejamiento (`### Pairing`)

- **Comando:** `switch2d pair`
- **Resultado:** **PASS**
- **Detalle:** Se emparejaron satisfactoriamente ambos mandos en un entorno previamente limpio:
  - `Joy-Con 2 (Left)` (`BC:89:A6:45:83:CC`) asignado a Player 1.
  - `Joy-Con 2 (Right)` (`BC:89:A6:49:8D:74`) asignado a Player 2.
- Ambos mandos quedaron registrados en `~/.config/switch2d/config.json`.
- `switch2d status` y `switch2d doctor` validaron la presencia de los 2 mandos en modo `Combined`.

---

## 4. Resultados de la Sección de Uso y Reconexión (`switch2d run`)

- **Resultado:** **FALLO EN LA RECONEXIÓN TRAS LA INSTALACIÓN**
- **Comportamiento observado:**
  1. Con el demonio corriendo mediante `switch2d run` (y sin instancias en conflicto ni Steam abierto), al pulsar botones para despertar los mandos emparejados, estos **no se descubren ni reconectan**.
  2. Los LEDs verdes de los Joy-Con no quedan fijos (comportamiento esperado descrito en la guía: *"Joy-Con only shows LEDs while a host is connected. switch2d run lights it again"*).
  3. La herramienta de diagnóstico `switch2d advert --seconds 15` reporta `[advert] no live advertisements seen. discovery stopped.`.

---

## 5. Hallazgos Críticos y Recomendaciones para el Equipo de Desarrollo

### 1. Ausencia de Exclusión Mutua / Detección de Instancias en `switch2d run`
- **Problema:** Si el usuario tiene habilitado o iniciado el servicio de usuario (`switch2d.service`) y simultáneamente corre `switch2d run` en la consola, **no hay ninguna advertencia ni bloqueo**. Ambos procesos compiten por el adaptador Bluetooth y el puerto DSU, provocando un fallo silencioso e impidiendo la conexión de los mandos.
- **Recomendación:** Implementar un mecanismo de instancia única (*singleton*) mediante lockfile (`$XDG_RUNTIME_DIR/switch2d.lock`) o verificar procesos previos antes de iniciar `run()`, abortando con un error explícito.

### 2. Vacíos en la Documentación del README
- **Control del proceso en primer plano:** El README no especifica cómo detener `switch2d run` (presionando `Ctrl + C`), lo que genera dudas sobre si el comando deja demonios huérfanos.
- **Flujo bajo demanda (*on-demand*):** Solo se menciona `systemctl --user enable` para arranque automático al login; no se documenta el uso de `systemctl --user start/stop switch2d` para usuarios que prefieren no tener el demonio consumiendo recursos o manteniendo pads virtuales activos permanentemente.
- **Rutas relativas en comandos `sudo`:** Los pasos 2 y 3 asumen que la terminal se mantiene en la raíz del repositorio (`system/70-switch2d.rules`). Si el usuario navega a otro directorio, los comandos fallan sin una indicación clara.

### 3. Falla de Reconexión de Mandos
- La promesa de la guía (*"The daemon dials a pad only when it sees it advertising... switch2d run lights it again"*) no se cumplió de forma transparente tras completar todos los pasos en una instalación limpia. El equipo de desarrollo debe revisar si el flujo de escaneo BLE de BlueZ (`busctl StartDiscovery`) o la política de filtrado de anuncios requiere ajustes o una instrucción adicional en la documentación para guiar al usuario si los mandos entran en suspensión profunda.
