# PerformanceHub · UI-Konzepte

Drei eigenständige Slint-Templates für Windows 11. Jedes läuft mit Beispieldaten im Viewer. Ausgangspunkt ist [`main` bei `64122ee`](https://github.com/Ishtvr/PerformanceHub/tree/64122ee7c6c5c995495f86801e35ed45102b8d5b), Version `2.0.0-alpha.1`.

| Konzept | Was in zwei Sekunden erkennbar ist | Bedienprobe | Ansichten |
| --- | --- | --- | --- |
| **[Relay](relay-flyout/README.md)** · Tray-Flyout | Zuletzt gewählter HP-Modus und beide Chiptemperaturen | „Leise“ wählen, „Kühlung“ öffnen, Drehzahlen ablesen | [Flyout](relay-flyout/vorschau.png) · [Kühlung](relay-flyout/kuehlung.png) |
| **[Signal](signal/README.md)** · Telemetrie | Aktuelle Temperatur und letzte Spitze von CPU und GPU auf derselben Skala | Zwischen 60 Sekunden und fünf Minuten wechseln, dann die Kompaktansicht öffnen | [Cockpit](signal/vorschau.png) · [Kompakt](signal/kompakt.png) |
| **[Werkbank](werkbank/README.md)** · Kurveneditor | Temperatur und Sollwert des gewählten Punkts sowie Abstand zur gespeicherten Kurve | Einen Punkt auswählen, um einen Prozentpunkt anheben, mit dem gespeicherten Stand vergleichen | [Editor](werkbank/vorschau.png) · [Schnellwahl](werkbank/schnellwahl.png) |

**Startempfehlung.** Relay zuerst als Tray-Einstieg erproben. Der häufige Ablauf „Modus wählen, Temperatur prüfen, schließen“ benötigt ein einziges kleines Fenster. Für diesen Schnitt existieren bereits Tray-Ereignisse, Modusbefehle und Sensor-Snapshots. Die wichtigste offene Designentscheidung ist, ob der Tray-Klick künftig das Flyout oder das heutige Hauptfenster öffnet. Signal bietet sich für den lesenden Diagnosepfad an, Werkbank für längere Änderungen an einer Kennlinie.

## Was auf `main` vorhanden ist

| Bereich | Im Repository | Bedeutung für die Entwürfe |
| --- | --- | --- |
| Desktop und Tray | Die App kompiliert `ui/app.slint` mit Slint 1.18.1. Ein Win32-Owner verarbeitet Tray-, Energie- und Worker-Ereignisse. Das Hauptfenster hat eigenes Chrome. [Build](../crates/app/build.rs) · [UI-Einstieg](../crates/app/src/slint_ui.rs) · [Architektur](../docs/ARCHITECTURE.md) | Die neuen Dateien haben eigene Fenster und importieren keinen Projektcode. Ein an der Taskleiste verankertes Relay-Flyout ist ein Vorschlag. |
| Profile und Kühlung | Vier HP-Modi, gespeicherte Leistungsprofile, eine gemeinsame Lüfterkennlinie, Automatik und Maximum sind implementiert. RPMs und die Anforderung eines Modus sind verschiedene Informationen. [HP-Modi](../crates/core/src/hp_mode.rs) · [Lüfter-Presets](../crates/core/src/fan_profile.rs) · [Kühlung](../docs/FAN_CONTROL.md) | Relay nennt die letzte Wahl. Werkbank trennt Entwurf, gespeicherten Sollwert und gemessene Drehzahlen. Eine physische Kurvenübernahme benötigt einen eigenen Nachweis. |
| Telemetrie | CPU-Last und OS-Takt kommen von Windows. Tctl, Package-Leistung und effektive Takte benötigen den passenden CPU-Leser. NVIDIA liefert Temperatur, Takt und Leistungsaufnahme. [Datenmodell](../crates/core/src/lib.rs) · [CPU-Leser](../docs/CPU_SENSORS.md) | Signal nutzt diese Felder. GPU-Auslastung und ein Zeitpuffer für Sensorverläufe wären Erweiterungen. Ein fehlender CPU-Leser lässt die unabhängigen Quellen weiter nutzbar. |
| Akku und Netz | Akkustand, Stromquelle und automatische Profilwahl bei Netzwechsel sind vorhanden. Im aktuellen Funktionskatalog gibt es keinen Regler für eine Akkuladegrenze. [Akkupolitik](../crates/core/src/battery.rs) · [Energieereignisse](../docs/POWER_LIFECYCLE.md) · [Katalog](../crates/core/src/features.rs) | Ein Ladelimit wäre eine Erweiterung mit gerätespezifischem Adapter, Fähigkeitenprüfung und Rücklesen. |
| GPU und Display | GPU-Leistungsgrenzen hängen von lesbaren Originalwerten und Gerätefähigkeiten ab. Windows kann die Bildrate des internen Panels auf unterstützte Modi stellen. Der HP-MUX ist eine BIOS-Vorgabe für den nächsten Start. [Windows-Steuerung](../docs/SYSTEM_CONTROLS.md) · [Hardwarematrix](../docs/HARDWARE_MATRIX.md) | Ein späteres Display-Modul sollte die gemeldeten Hz anbieten und einen ausstehenden Neustart ausdrücklich zeigen. Der Kurveneditor im Beispiel bearbeitet eine Lüfterkennlinie. |

Als Gerätebeispiel dient der im Projekt dokumentierte HP OMEN 16 mit Board 8E35, Ryzen 9 8940HX und GeForce RTX 5070 Laptop. Alle Zahlen in den drei Templates sind erfunden. Die Screenshots sind Ansichten dreier Alternativen mit jeweils eigenem Beispielzustand.

## Windows 11 und Laufzeit

- **Renderer.** PerformanceHub wählt standardmäßig `winit-software`. Das App-Manifest aktiviert außerdem FemtoVG. Software ist der sinnvolle erste Messpunkt für wenige Flächen und kurze Pfade. Beim Vergleich sollten Scrollen und Kurvenbearbeitung im offenen Fenster sowie CPU, Arbeitsspeicher und GPU-Aktivität nach dem Verstecken getrennt gemessen werden. Skia unterstützt unter Windows unter anderem Direct3D, erfordert hier aber zusätzliche Build-Features. Ein Benchmark der produktiven App steht aus. [Backend-Auswahl](../crates/app/src/slint_ui.rs) · [Manifest](../crates/app/Cargo.toml) · [Winit-Renderer](https://releases.slint.dev/1.18.1/docs/slint/guide/backends-and-renderers/backend_winit/)
- **Leerlauf.** Die Templates haben keine periodischen Timer. Bei späterer Anbindung liefern die bestehenden Worker sichtbare Messwerte. Die Host-UI stoppt beim Verstecken ihre Anzeige-Timer und meldet die geänderte Sichtbarkeit an den Worker. Eine aktive Kühlungsregelung hat ihren eigenen Lebenszyklus. [UI-Host](../crates/app/src/slint_ui.rs) · [Pacing](../crates/app/src/slint_ui/pacing.rs)
- **Fenster und Skalierung.** Relay ist rahmenlos, Signal und Werkbank verwenden die normale Fensterdekoration. Für Relay im Produkt liefert `Shell_NotifyIconGetRect` den Tray-Anker. Der Host muss das Fenster an die Monitor-Arbeitsfläche anpassen, Fokus zurückgeben und mit einem Symbol im Überlauf umgehen. DPI-Wechsel betreffen auch Zeigerkoordinaten und Kurven-Trefferflächen. Ein späterer eigener Maximieren-Knopf braucht korrektes Hit-Testing für Snap-Layouts. [Windows-Tray](https://learn.microsoft.com/en-us/windows/win32/shell/notification-area) · [DPI-Wechsel](https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-dpichanged) · [Snap-Layouts](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/ui/apply-snap-layout-menu)
- **Material.** Die Screens verwenden deckende Farben. Echtes Mica benötigt einen nativen Systemhintergrund und eine passende Fensterkomposition. Ein transparenter Slint-Hintergrund allein liefert diesen Effekt nicht. [Mica in Win32](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/ui/apply-mica-win32)

## Vorschau und Prüfung

Ein Template im Slint-MCP per absolutem Pfad an `check_slint` übergeben oder vom Repository-Stamm mit dem [offiziellen Windows-Viewer 1.18.1](https://github.com/slint-ui/slint/releases/tag/v1.18.1) öffnen:

```powershell
slint-viewer ui-templates/relay-flyout/main.slint
slint-viewer ui-templates/signal/main.slint
slint-viewer ui-templates/werkbank/main.slint
```

Alle drei Einstiege wurden mit dem **Slint-Preview-MCP 1.19.1** kompiliert und anhand seiner PNGs auf Ausrichtung, Beschnitt, Auswahl und Lesbarkeit geprüft. Die sechs verlinkten Bilder stammen aus dem **Windows-Viewer 1.18.1** im Screenshot-Modus. Sie zeigen je Konzept zwei reproduzierbare Zustände. Zusätzlich wurde in Signal der Ausfall des CPU-Hardwarelesers im Ein-Minuten-Fenster geprüft. Sichtbar bleiben GPU-Kurve, GPU-Werte und Windows-CPU-Last. Die Bildprüfung deckt den Fensterinhalt ab. Native Fokusübergabe, Monitorwechsel und Ressourcenverbrauch sind am integrierten Fenster zu prüfen.

Für die nächste Iteration genügen der offizielle **Slint-Skill**, **`slint-docs`** mit `version="1.18.1"` und **`slint-preview`**. Die priorisierte Recherche nennt Quellen, Pflegebelege und Grenzen der ergänzenden Design- und Review-Werkzeuge. [Werkzeugbewertung](WERKZEUGE.md)
