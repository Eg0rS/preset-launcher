# HTTP API

Локальный API для удалённого управления ПК. Включается в окне программы (блок «Удалённое управление»), порт по умолчанию **47800**, сервер слушает `0.0.0.0`. Включение и смена порта применяются без перезапуска. Протокол - HTTP без TLS, только для локальной сети (см. [README, «Безопасность»](../README.md#безопасность)).

В примерах: `192.168.1.10` - адрес ПК, `<токен>` - токен из окна программы.

## Общие правила

- Тела запросов и ответов - JSON (UTF-8). Ответы приходят с `Content-Type: application/json; charset=utf-8`.
- Ошибки имеют вид `{"error": "<код>"}`.
- Строка запроса после `?` игнорируется. Сопоставление пути и метода точное: другой метод или путь (в том числе со слэшем в конце) - `404`.
- Неизвестные поля в JSON-телах игнорируются.

### Аутентификация

Каждый запрос должен содержать заголовок:

```
Authorization: Bearer <токен>
```

Схема `Bearer` нечувствительна к регистру, токен - чувствителен. Пустой токен в настройках никого не авторизует. Проверка идёт **до** разбора пути и чтения тела, поэтому без верного токена любой запрос (даже на несуществующий путь) получает `401 {"error":"unauthorized"}`. «Новый токен» в окне программы сразу отменяет прежний.

### Ограничения

| Что | Предел | Результат при превышении |
|---|---|---|
| размер тела запроса | 16 КБ (16384 байт) | `400 bad request` |
| тело короче заявленного `Content-Length` (обрыв) | - | `400 bad request`, команда не выполняется |
| одновременные запросы | 8 | `503 {"error":"busy"}` |
| одновременно показываемые уведомления | 4 | `503 {"error":"busy"}` |

### Сводка кодов

| Код | Значение |
|---|---|
| `200` | выполнено, результат в теле |
| `202` | принято, выполнится в фоне или по таймеру |
| `400` | `bad request`: битый JSON, нет обязательного поля, значение вне диапазона или неверного типа, слишком большое тело |
| `401` | `unauthorized` |
| `404` | `unknown preset` (только `/api/switch`) или `not found` |
| `500` | операция не удалась (`shutdown failed`, `power failed`, `volume failed`, `media failed`, `notify failed`); подробности в журнале |
| `501` | `not supported`: действие не поддерживается на этой ОС (только macOS, см. ниже) |
| `503` | `busy`: превышен лимит запросов или уведомлений |

## Эндпоинты

| Метод и путь | Назначение |
|---|---|
| `GET /api/status` | состояние лаунчера, ПК и звука |
| `POST /api/switch` | переключить пресет |
| `POST /api/shutdown` | выключить ПК (с задержкой) |
| `POST /api/power` | сон, гибернация, блокировка, перезагрузка |
| `POST /api/shutdown/cancel` | отменить ожидающее действие питания |
| `POST /api/volume` | громкость и mute |
| `POST /api/media` | медиаклавиша |
| `POST /api/notify` | уведомление Windows |

### GET /api/status

Тела нет.

```json
{
  "active": "Игры",
  "busy": false,
  "presets": ["Работа", "Игры"],
  "version": "0.3.0",
  "shutdown_pending": false,
  "pending_power": null,
  "volume": 42,
  "muted": false
}
```

| Поле | Тип | Описание |
|---|---|---|
| `active` | строка или `null` | активный пресет (`null`, если ещё ничего не применялось) |
| `busy` | bool | идёт переключение |
| `presets` | массив строк | имена пресетов |
| `version` | строка | версия программы |
| `shutdown_pending` | bool | запланировано любое действие питания |
| `pending_power` | `null` \| `"shutdown"` \| `"sleep"` \| `"hibernate"` \| `"lock"` \| `"restart"` | какое именно |
| `volume` | целое 0-100 или `null` | громкость устройства вывода по умолчанию; `null` при ошибке Core Audio |
| `muted` | bool или `null` | mute; `null` при ошибке Core Audio |

```bash
curl -H "Authorization: Bearer <токен>" http://192.168.1.10:47800/api/status
```

```powershell
$h = @{ Authorization = "Bearer <токен>" }
Invoke-RestMethod http://192.168.1.10:47800/api/status -Headers $h
```

### POST /api/switch

Тело: `{"preset": "<имя>"}`; `preset` - строка, обязательна, точное совпадение с именем пресета (регистр учитывается).

| Ответ | Когда |
|---|---|
| `202 {"accepted":true,"preset":"Игры"}` | команда принята, переключение идёт в фоне |
| `404 {"error":"unknown preset"}` | такого пресета нет |
| `400 {"error":"bad request"}` | битый JSON, нет поля `preset` или оно не строка |

Новая команда прерывает текущее переключение (оставшиеся программы не запускаются), промежуточные команды в очереди отбрасываются. Успешно принятый `switch` **отменяет** ожидающее действие питания; запрос с неизвестным пресетом его не отменяет.

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"preset":"Работа"}' http://192.168.1.10:47800/api/switch
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/switch -Method Post -Headers $h `
  -ContentType 'application/json; charset=utf-8' -Body '{"preset":"Работа"}'
```

### POST /api/shutdown

Тело необязательно: пустое или JSON-объект с необязательным `delay_secs` - целое 0-600 (по умолчанию **10**). Всё остальное (не объект, дробное, отрицательное, строка, `null`, больше 600) - `400`.

| Ответ | Когда |
|---|---|
| `202 {"accepted":true,"delay_secs":10}` | выключение запланировано (или запущено при `0`) |
| `500 {"error":"shutdown failed"}` | при `delay_secs = 0` Windows отказала; при ненулевой задержке ошибка выполнения пишется только в журнал |

Задержку отсчитывает сам лаунчер (отменяемый таймер). По истечении выполняется `shutdown.exe /s /t 0` **без `/f`**: программы с несохранёнными данными могут задержать выключение, но не теряют их.

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"delay_secs":30}' http://192.168.1.10:47800/api/shutdown
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/shutdown -Method Post -Headers $h `
  -ContentType 'application/json' -Body '{"delay_secs":30}'
```

### POST /api/power

Тело: JSON-объект.

| Поле | Тип | Обязательно | Описание |
|---|---|---|---|
| `action` | строка | да | `sleep`, `hibernate`, `lock`, `restart` (строго в нижнем регистре; для выключения есть `/api/shutdown`) |
| `delay_secs` | целое 0-600 | нет | задержка; по умолчанию **0 для `lock`**, **10** для остальных |

| action | Что выполняется |
|---|---|
| `sleep` | `SetSuspendState(FALSE)`; при включённой гибернации Windows может уйти в гибернацию |
| `hibernate` | `SetSuspendState(TRUE)`; если гибернация отключена в системе, не сработает |
| `lock` | `LockWorkStation` |
| `restart` | `shutdown.exe /r /t 0` без `/f` |

| Ответ | Когда |
|---|---|
| `202 {"accepted":true,"action":"sleep","delay_secs":10}` | принято |
| `400 {"error":"bad request"}` | нет или неизвестный `action`, `delay_secs` вне 0-600 или не целое |
| `500 {"error":"power failed"}` | при `delay_secs = 0` команда не выполнилась (`lock`, `restart`) |

`sleep` и `hibernate` выполняются в отдельном потоке с паузой около секунды, чтобы ответ успел уйти; ответ `202` приходит сразу, возможный отказ виден только в журнале.

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"action":"sleep","delay_secs":30}' http://192.168.1.10:47800/api/power
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/power -Method Post -Headers $h `
  -ContentType 'application/json' -Body '{"action":"lock"}'
```

### POST /api/shutdown/cancel

Тела нет (игнорируется). Отменяет любое ожидающее действие питания.

```json
200 {"cancelled": true}
```

`cancelled: false` - отменять было нечего. Результат `200` в обоих случаях.

```bash
curl -X POST -H "Authorization: Bearer <токен>" http://192.168.1.10:47800/api/shutdown/cancel
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/shutdown/cancel -Method Post -Headers $h
```

### Семантика отложенных действий питания

- Одновременно ожидает **не более одного** действия на все виды питания (`shutdown`, `sleep`, `hibernate`, `lock`, `restart`). Новый запрос с ненулевой задержкой заменяет прежнее ожидающее; запрос с `delay_secs = 0` отменяет ожидающее и выполняется немедленно.
- Ожидающее действие видно в `GET /api/status` (`shutdown_pending`, `pending_power`).
- Его отменяют `POST /api/shutdown/cancel` и успешно принятый `POST /api/switch`.
- Таймер живёт в процессе лаунчера: если программу закрыть, ожидающее действие пропадает.
- Ошибка действия по таймеру в ответ не попадает (запрос давно завершён), она пишется в журнал.

### POST /api/volume

Устройство - вывод по умолчанию (Core Audio). Тело: JSON-объект, **хотя бы одно** из полей:

| Поле | Тип | Описание |
|---|---|---|
| `level` | целое 0-100 | абсолютная громкость |
| `delta` | целое от -100 до 100 | изменение относительно текущей; результат ограничивается 0-100 |
| `muted` | bool | включить или выключить mute |

`level` и `delta` вместе недопустимы. Значения вне диапазона, дробные, строки и `null` дают `400`.

| Ответ | Когда |
|---|---|
| `200 {"volume":30,"muted":false}` | итоговые значения |
| `400 {"error":"bad request"}` | пустое тело, ни одного поля, `level` + `delta`, неверные значения |
| `500 {"error":"volume failed"}` | ошибка Core Audio |

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"delta":-10}' http://192.168.1.10:47800/api/volume
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/volume -Method Post -Headers $h `
  -ContentType 'application/json' -Body '{"level":30,"muted":false}'
```

### POST /api/media

Тело: `{"action": "<действие>"}`, где действие - `play_pause`, `next`, `prev` или `stop`. Лаунчер эмулирует нажатие медиаклавиши (`SendInput`, нажатие и отпускание); реагирует активный медиаплеер.

| Ответ | Когда |
|---|---|
| `200 {"ok":true}` | клавиша отправлена |
| `400 {"error":"bad request"}` | нет или неизвестный `action` |
| `500 {"error":"media failed"}` | `SendInput` не удался |

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"action":"play_pause"}' http://192.168.1.10:47800/api/media
```

```powershell
Invoke-RestMethod http://192.168.1.10:47800/api/media -Method Post -Headers $h `
  -ContentType 'application/json' -Body '{"action":"next"}'
```

### POST /api/notify

Тело: `{"title": "...", "message": "..."}`.

| Поле | Тип | Границы |
|---|---|---|
| `title` | строка | 1-100 символов Unicode |
| `message` | строка | 1-500 символов Unicode |

Строка не может состоять только из пробелов и символов, недопустимых в XML 1.0 (например управляющих). Текст экранируется как XML, разметка не интерпретируется.

| Ответ | Когда |
|---|---|
| `202 {"accepted":true}` | показ запущен в фоне |
| `400 {"error":"bad request"}` | поле отсутствует, пустое, неверного типа или длиннее лимита |
| `503 {"error":"busy"}` | уже показывается 4 уведомления |
| `500 {"error":"notify failed"}` | не удалось запустить показ |

Уведомление - Windows toast от имени приложения (`AppUserModelID = PresetLauncher.App`), для которого нужна установка через установщик (ярлык в меню «Пуск»). Без ярлыка (например при `cargo run`) Windows может не показать тост, при этом ответ всё равно `202`: ошибка самого показа пишется только в журнал.

```bash
curl -X POST -H "Authorization: Bearer <токен>" -H "Content-Type: application/json" \
  -d '{"title":"Привет","message":"Ужин готов"}' http://192.168.1.10:47800/api/notify
```

```powershell
$body = @{ title = "Привет"; message = "Ужин готов" } | ConvertTo-Json
Invoke-RestMethod http://192.168.1.10:47800/api/notify -Method Post -Headers $h `
  -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($body))
```

Для кириллицы в PowerShell 5.1 передавайте тело байтами UTF-8, как в последнем примере; иначе возможна неверная кодировка.

## Различия на macOS

На macOS (см. [MACOS.md](MACOS.md)) API тот же, но часть действий не поддерживается. Такие запросы получают **`501 {"error":"not supported"}`**; задержка при этом не ставится.

| Запрос | macOS |
|---|---|
| `POST /api/power` `{"action":"hibernate"}` | `501`: гибернации нет |
| `POST /api/media` `{"action":"stop"}` | `501`: у macOS нет системной клавиши Stop |
| `POST /api/shutdown`, `/api/power` (`restart`, `sleep`, `lock`) | работают: `osascript` (System Events), `pmset`; `lock` требует разрешений «Универсальный доступ» и «Автоматизация»; без них `500` (запасного гашения дисплея нет) |
| `POST /api/media` (`play_pause`, `next`, `prev`) | системные media-события; без разрешения «Универсальный доступ» - `500` (подсказка в журнале) |
| `POST /api/volume`, `GET /api/status` (громкость) | через `osascript`; у устройств без регулировки громкости (часть HDMI) вернётся `volume`: `null` в статусе и `500` при записи |
| `POST /api/notify` | уведомление Центра уведомлений (показывается от имени «Редактора скриптов») |

Остальное (`status`, `switch`, `shutdown/cancel`) без отличий.

## Интеграция с Home Assistant

Положите файл в `/config/packages/pc.yaml` и включите пакеты в `configuration.yaml`:

```yaml
homeassistant:
  packages: !include_dir_named packages
```

Секреты в `/config/secrets.yaml`:

```yaml
launcher_auth: "Bearer <токен>"
pc_mac: "AA:BB:CC:DD:EE:FF"   # MAC сетевой карты ПК (для Wake-on-LAN)
```

### Сенсоры (rest)

Статус опрашивается раз в 15 секунд. Если ПК выключен или лаунчер не запущен, сенсоры становятся `unavailable`.

```yaml
rest:
  - resource: http://192.168.1.10:47800/api/status
    headers:
      Authorization: !secret launcher_auth
    scan_interval: 15
    timeout: 3
    sensor:
      - name: "PC preset"
        unique_id: pc_preset
        icon: mdi:application-cog
        value_template: "{{ value_json.active or 'нет' }}"
        json_attributes:
          - presets
      - name: "PC volume"
        unique_id: pc_volume
        icon: mdi:volume-high
        unit_of_measurement: "%"
        value_template: "{{ value_json.volume if value_json.volume is not none else none }}"
      - name: "PC pending power"
        unique_id: pc_pending_power
        icon: mdi:timer-sand
        value_template: "{{ value_json.pending_power or 'нет' }}"
    binary_sensor:
      - name: "PC shutdown pending"
        unique_id: pc_shutdown_pending
        value_template: "{{ value_json.shutdown_pending }}"
      - name: "PC muted"
        unique_id: pc_muted
        icon: mdi:volume-off
        value_template: "{{ value_json.muted | default(false) }}"
```

### Команды (rest_command)

```yaml
rest_command:
  pc_switch:
    url: http://192.168.1.10:47800/api/switch
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"preset": preset} | to_json }}'
    timeout: 5
  pc_shutdown:
    url: http://192.168.1.10:47800/api/shutdown
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{"delay_secs": 10}'
    timeout: 5
  pc_shutdown_cancel:
    url: http://192.168.1.10:47800/api/shutdown/cancel
    method: post
    headers:
      Authorization: !secret launcher_auth
    timeout: 5
  # cmd: sleep | hibernate | lock | restart; delay_secs 0-600 (отменяется pc_shutdown_cancel)
  pc_power:
    url: http://192.168.1.10:47800/api/power
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"action": cmd, "delay_secs": (delay_secs | default(0)) | int} | to_json }}'
    timeout: 5
  pc_volume_set:
    url: http://192.168.1.10:47800/api/volume
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"level": ([0, (level | float(0)) | round(0) | int, 100] | sort)[1]} | to_json }}'
    timeout: 5
  pc_volume_step:
    url: http://192.168.1.10:47800/api/volume
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"delta": (delta | int(0))} | to_json }}'
    timeout: 5
  pc_volume_mute:
    url: http://192.168.1.10:47800/api/volume
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"muted": muted in [true, "true", "on", 1, "1"]} | to_json }}'
    timeout: 5
  # cmd: play_pause | next | prev | stop
  pc_media:
    url: http://192.168.1.10:47800/api/media
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"action": cmd} | to_json }}'
    timeout: 5
  pc_notify:
    url: http://192.168.1.10:47800/api/notify
    method: post
    headers:
      Authorization: !secret launcher_auth
    content_type: application/json
    payload: '{{ {"title": (title | default("Home Assistant"))[:100], "message": message[:500]} | to_json }}'
    timeout: 5
```

### Примеры использования

Переключить пресет и выключить ПК с возможностью отмены:

```yaml
script:
  pc_game_mode:
    alias: "ПК: игровой режим"
    sequence:
      - action: rest_command.pc_switch
        data:
          preset: "Игры"

  pc_off:
    alias: "ПК: выключить"
    sequence:
      - action: rest_command.pc_shutdown
      - action: rest_command.pc_notify
        data:
          title: "Выключение"
          message: "Компьютер выключится через 10 секунд."
```

Включение ПК: лаунчер работает только на запущенной Windows, поэтому из выключенного состояния ПК будит Wake-on-LAN (интеграция `wake_on_lan`, MAC из `secrets.yaml`). Когда сенсор `sensor.pc_preset` снова станет доступным, можно отправить `rest_command.pc_switch`. Если ПК ждёт выключения, `rest_command.pc_shutdown_cancel` отменяет его.

Совет: `rest_command` ждёт ответа не дольше `timeout`. Долгие сценарии (пробуждение ПК может занять минуты) запускайте через `script.turn_on`: он не блокирует вызывающего, и голосовой ассистент получает ответ сразу.
