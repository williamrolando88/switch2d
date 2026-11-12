# Reporte de Instalación Paso a Paso (Ronda 2: Guía Actualizada) - switch2d

Este informe documenta la segunda evaluación de la guía de instalación y uso de `switch2d`, tras la reestructuración completa de [README.md](file:///var/home/wmorales/repos/switch2d/README.md), la incorporación del instalador unificado `install.sh` y las nuevas comprobaciones diagnósticas en `switch2d doctor`.

**Reglas de la evaluación:**
- Mantener la perspectiva estricta de usuario final.
- Ejecutar paso a paso conforme a la guía.
- Reportar de forma detallada éxitos, advertencias y bloqueos sin reparar proactivamente.

---

## 1. Información del Entorno

- **Fecha:** 2026-11-12
- **Usuario:** `wmorales`
- **Directorio de trabajo:** `/var/home/wmorales/repos/switch2d`
- **Novedades en el repositorio:**
  - `install.sh`: Instalador unificado en bash con soporte para `--service` y `--uninstall`.
  - `doctor`: Agregó comprobación de D-Bus Discovery, regla NOPASSWD de sudoers y unidades legacy `nso-gc`.
  - `README.md`: Documentación reescrita desde cero orientada a usuario final.

---

## 2. Bitácora de Pasos Ejecutados

### Paso 0: Desactivación de Unidades del Bridge Anterior (Python `nso-gc`)
```bash
systemctl --user disable --now nso-gc-watchdog.timer
systemctl --user disable --now nso-gc.service nso-gc-watchdog.service nso-gc-after-gamescope.service
```
- **Ejecutado por:** Usuario
- **Resultado:** **ÉXITO**
- **Verificación:** Unidades desactivadas e inactivas.

---

### Paso 1: Instalación Unificada del Programa
```bash
./install.sh
```
- **Ejecutado por:** Usuario (terminal interactiva con elevación `sudo`)
- **Resultado:** **ÉXITO (Exit Code 0)**
- **Verificación de componentes:**
  - Binario: `/home/wmorales/.cargo/bin/switch2d` instalado y ejecutable.
  - Regla udev: `/etc/udev/rules.d/70-switch2d.rules` instalada con permisos 644.
  - LE Privacy: `/etc/systemd/system/switch2d-disable-le-privacy.service` y `/usr/local/bin/disable-le-privacy.sh` instalados y activos (`active (exited)`).
  - Sudoers: `/etc/sudoers.d/switch2d-btmgmt` validado con `visudo -cf`.

---

### Paso 2: Verificación de la Instalación (`switch2d doctor`)
```bash
switch2d doctor
```
- **Resultado:** **EXIT CODE 0 (Con discrepancias / Hallazgos de UX)**
- **Salida obtenida:**
  ```text
   INFO Loaded config from /home/wmorales/.config/switch2d/config.json
  PASS  uinput: /dev/uinput is writable
  PASS  le-privacy unit: switch2d-disable-le-privacy.service: is-enabled=enabled, is-active=active
  PASS  bluetooth adapter: bazzite (8C:17:..:D6:1E) [MAC partially redacted]
  PASS  discovery: StartDiscovery accepted (adapter is discoverable); Device1 objects: 6, with ManufacturerData: 3
  PASS  sudo btmgmt: sudo -n -l /usr/bin/btmgmt is allowed (no password prompt)
  SKIP  legacy bridge units: could not determine all nso-gc unit states
    fix: run `systemctl --user list-unit-files 'nso-gc*'` as the logged-in user
  PASS  running instances: no other switch2d process
  SKIP  user unit: could not read switch2d.service: systemctl --user cat switch2d.service exited with exit status: 1: No files found for switch2d.service.
    fix: run `systemctl --user cat switch2d.service` as the logged-in user
  PASS  DSU port: 127.0.0.1:26760 is free
  PASS  config: /home/wmorales/.config/switch2d/config.json parsed; 2 bonded controller(s)
  PASS  udev uaccess rule: found /etc/udev/rules.d/70-switch2d.rules
  ```

---

### Paso 3: Emparejamiento de Mandos (`## Pair your controllers`)
```bash
switch2d pair
```
- **Ejecutado por:** Usuario
- **Resultado:** **ÉXITO TOTAL**
- **Verificación:**
  - Ambos mandos detectados y guardados en `~/.config/switch2d/config.json`:
    - `Joy-Con 2 (Left)` (`BC:89:A6:45:83:CC`) - Player 1
    - `Joy-Con 2 (Right)` (`BC:89:A6:49:8D:74`) - Player 2
  - `switch2d status` y `switch2d doctor` confirmaron la presencia de 2 bonded controllers.
  - La nueva prueba de descubrimiento D-Bus reporta: `Device1 objects: 6, with ManufacturerData: 3`.

---

### Paso 4: Ejecución y Reconexión (`## Run it`)
```bash
switch2d run
```
- **Ejecutado por:** Usuario
- **Resultado:** **ÉXITO TOTAL**
- **Comportamiento:** Ambos mandos despertaron al pulsar botones, establecieron la conexión L2CAP con el demonio y fijaron sus luces de jugador en sólido.

---

### Paso 5: Diagnóstico con Monitor (`## Check that it works`)
```bash
switch2d monitor --seconds 30
```
- **Ejecutado por:** Usuario
- **Resultado:** **HALLAZGO DE DOCUMENTACIÓN Y UX (Comportamiento Mono-Controlador)**
- **Hallazgo registrado:**
  - El usuario reportó que el segundo Joy-Con no conectaba con `switch2d monitor`.
  - **Causa en el código (`src/main.rs:228`):** Por diseño, el comando `monitor` se conecta a **un solo mando** a la vez (`config.controllers.first()`, que corresponde al Joy-Con 2 Left).
  - **Vacío en el README:** El README no explica que `monitor` es para probar un único mando ni que requiere especificar la MAC (`switch2d monitor <MAC>`) para monitorear el segundo Joy-Con.

---

### Paso 6: Cambio y Validación de Modos (`## Two controllers as one, or as two`)
```bash
switch2d mode split
switch2d mode combined
```
- **Ejecutado por:** Usuario
- **Resultado:** **ÉXITO TOTAL CONFIRMADO**
- **Validación:**
  - **Modo `split`:** Crea dos dispositivos virtuales independientes en `/dev/uinput` (`Joy-Con 2 Left` y `Right`), asigna Player 1 y Player 2 en los LEDs correspondientes y orienta el mapeo horizontalmente.
  - **Modo `combined`:** Unifica ambos mandos en un solo nodo (`Combined Joy-Cons`) y sincroniza los LEDs de ambos al Player 1.
  - La alternancia entre modos se realiza de forma limpia y se refleja en `config.json`.

---

### Paso 7: Prueba en Juego Real y Mapeo de Botones (`## Playing games`)
- **Ejecutado por:** Usuario (juego real con el demonio en segundo plano vía `systemctl --user start switch2d`)
- **Resultado:** **JUEGO FUNCIONAL CON INVERSIÓN COMPLETA DEL ROMBO DE BOTONES (A/B y X/Y)**
- **Observación del usuario:** El mando funciona correctamente en el juego, pero los botones **A** y **B** están invertidos (y por extensión todo el rombo de acción).
- **Análisis exhaustivo del mapeo físico vs evdev:**
  
  En un mando de Nintendo, la orientación cardinal física de los cuatro botones frontales (face buttons) es:
  - **N (Norte / 12h / Arriba):** Botón **`X`**
  - **S (Sur / 6h / Abajo):** Botón **`B`**
  - **E (Este / 3h / Derecha):** Botón **`A`**
  - **O (Oeste / 9h / Izquierda):** Botón **`Y`**

  Sin embargo, en el código fuente de `switch2d` (`src/daemon.rs:1078-1081`, `src/uinput/combined.rs:62-65` y `src/uinput/gamepad.rs:390-393`):
  ```rust
  // Mapeo actual en switch2d:
  gamepad.set_key(Key::BTN_SOUTH, pressed(button_mask::A)); // ❌ Asigna Sur al botón A
  gamepad.set_key(Key::BTN_EAST,  pressed(button_mask::B)); // ❌ Asigna Este al botón B
  gamepad.set_key(Key::BTN_WEST,  pressed(button_mask::X)); // ❌ Asigna Oeste al botón X
  gamepad.set_key(Key::BTN_NORTH, pressed(button_mask::Y)); // ❌ Asigna Norte al botón Y
  ```

  **Origen del error:**
  El código actual mapeó los botones basándose en los nombres de las letras de un mando de **Xbox** (donde `BTN_SOUTH` es la letra A y `BTN_EAST` es la letra B), en lugar de respetar la posición cardinal del estándar evdev (`<linux/input-event-codes.h>`):
  - Al pulsar el botón físico **A** (Este), el sistema emite `BTN_SOUTH` (acción inferior/Sur).
  - Al pulsar el botón físico **B** (Sur), el sistema emite `BTN_EAST` (acción derecha/Este).
  - Al pulsar el botón físico **X** (Norte), el sistema emite `BTN_WEST` (acción izquierda/Oeste).
  - Al pulsar el botón físico **Y** (Oeste), el sistema emite `BTN_NORTH` (acción superior/Norte).

  **Corrección posicional requerida (N=X, S=B, E=A, O=Y):**
  ```rust
  gamepad.set_key(Key::BTN_NORTH, pressed(button_mask::X)); // Norte = X
  gamepad.set_key(Key::BTN_SOUTH, pressed(button_mask::B)); // Sur   = B
  gamepad.set_key(Key::BTN_EAST,  pressed(button_mask::A)); // Este  = A
  gamepad.set_key(Key::BTN_WEST,  pressed(button_mask::Y)); // Oeste = Y
  ```

---

### Paso 8: Desinstalación Completa (`## Uninstall`)
```bash
./install.sh --uninstall
cargo uninstall switch2d
rm -rf ~/.config/switch2d
```
- **Ejecutado por:** Usuario
- **Resultado:** **ÉXITO TOTAL (Clean Slate Verificado)**
- **Verificación:**
  - Binario: Desinstalado (`command -v switch2d` -> no encontrado).
  - Regla udev: `/etc/udev/rules.d/70-switch2d.rules` eliminada.
  - LE Privacy: Servicio y script eliminados del sistema (`/etc/systemd/system/` y `/usr/local/bin/`).
  - Sudoers: Regla `/etc/sudoers.d/switch2d-btmgmt` eliminada.
  - Servicio de usuario: `/home/wmorales/.config/systemd/user/switch2d.service` eliminado.
  - Directorio de configuración: `~/.config/switch2d/` eliminado.

---

## 3. Resumen Final de Hallazgos y Recomendaciones para el Equipo

1. **Corrección de Mapeo del Rombo de Botones (N=X, S=B, E=A, O=Y):**
   - En `src/daemon.rs`, `src/uinput/combined.rs` y `src/uinput/gamepad.rs`, cambiar la asignación de letras estilo Xbox por la posición geométrica real de Nintendo:
     - `BTN_NORTH -> X`
     - `BTN_SOUTH -> B`
     - `BTN_EAST  -> A`
     - `BTN_WEST  -> Y`
   - Esto garantizará que en juegos nativos, emuladores y Steam Input, la posición física coincida 1:1 con las acciones esperadas en pantalla.
2. **`switch2d monitor` es uniproceso/monocontrolador:**
   - Sugerencia para el README: Aclarar explícitamente en la sección `## Check that it works` que `monitor` evalúa un solo mando a la vez y documentar el uso de `switch2d monitor <MAC>`.
3. **`SKIP` espurio en `legacy bridge units` de `switch2d doctor`:**
   - `parse_enabled` en `src/doctor.rs` debe soportar el estado `"static"` que devuelve systemd para servicios auxiliares sin bloque `[Install]`, evitando un falso `SKIP`.
4. **`SKIP` en `user unit` sin `--service`:**
   - Cuando el usuario decide no instalar el servicio login, `doctor` debería mostrar `PASS (no instalado/opcional)` en lugar de `SKIP` con sugerencia de fix.
5. **Eficacia del instalador unificado (`install.sh`):**
   - El nuevo script automatiza de forma limpia la instalación, comprobación y desinstalación, resolviendo de raíz los tropiezos de rutas relativas y permisos de la primera versión.
