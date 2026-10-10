# lafe-backup

Malý zálohovací nástroj v Rustu. Vybrané složky a soubory zabalí do jednoho
`.tar.gz` archivu s časovou značkou v cílové složce (typicky na připojeném
síťovém disku) a staré archivy průběžně maže.

- nastavení přes interaktivní rozhraní v terminálu nebo přes příkazy
- archiv zachovává práva, vlastníky i symlinky (symlinky se nenásledují)
- pokud cíl leží na připojeném disku a ten zrovna připojený není, záloha skončí
  chybou a na lokální disk nic nezapíše
- archiv se píše jako `.partial` a přejmenuje se až po úspěšném dokončení

## Instalace

Je potřeba [Rust](https://rustup.rs) a C linker (`gcc`).

```sh
sh install.sh
```

Skript sestaví release verzi (do `~/.cache/lafe-backup-target`, aby to šlo i ze
síťového disku bez práva spouštění) a nainstaluje ji do `~/.local/bin/lafe-backup`.
Stejně se instaluje i každá další aktualizace.

## Použití

Samotné `lafe-backup` otevře interaktivní nastavení:

| Klávesa | Akce |
| --- | --- |
| `↑` `↓` / `j` `k` | pohyb |
| `Enter` | upravit řádek, otevřít prohlížeč složek |
| `mezerník` | v prohlížeči přidat/odebrat zdroj, vybrat cíl |
| `←` `→` | v prohlížeči o úroveň výš / do složky |
| `.` | zobrazit skryté soubory |
| `e` | napsat cestu ručně |
| `d` | odebrat zdroj nebo vynechané jméno |
| `r` | spustit zálohu |
| `q` | konec |

Totéž jde udělat příkazy:

```
lafe-backup add <cesta>...       přidat složky/soubory k zálohování
lafe-backup remove <cesta>...    odebrat je ze zálohování
lafe-backup dest <složka>        nastavit, kam se archivy ukládají
lafe-backup exclude <jméno>...   přeskakovat soubory/složky s tímto jménem
lafe-backup unexclude <jméno>... přestat je přeskakovat
lafe-backup keep <n>             kolik posledních archivů ponechat (0 = nemazat)
lafe-backup name <jméno>         prefix archivu -> <jméno>-RRRRMMDD-HHMMSS.tar.gz
lafe-backup schedule <kdy>       jak často zálohovat (viz níže)
lafe-backup show | list          vypsat aktuální nastavení
lafe-backup run [--dry-run]      provést zálohu (--dry-run jen vypíše, co by se zálohovalo)
```

Nastavení se ukládá do `~/.config/lafe-backup/config` (jinou cestu určí
proměnná `LAFE_BACKUP_CONFIG`).

## Automatické spouštění

Četnost se nastavuje řádkem „Četnost“ v interaktivním rozhraní nebo příkazem
`schedule`, který vygeneruje a zapne systemd timer uživatele
(`~/.config/systemd/user/lafe-backup.{service,timer}`):

```sh
lafe-backup schedule daily 03:00   # každý den ve 03:00
lafe-backup schedule 6h            # každých 6 hodin
lafe-backup schedule hourly        # každou hodinu
lafe-backup schedule Mon 02:30     # každé pondělí ve 02:30
lafe-backup schedule off           # vypnout automatické zálohy
sudo loginctl enable-linger "$USER"   # jednou: aby timer běžel i bez přihlášení
```

Projde i jakýkoli jiný výraz systemd `OnCalendar` (`man systemd.time`). Při
častějších zálohách nezapomeňte zvýšit `keep` – počítá archivy, ne dny.

## Obnova

Archiv obsahuje absolutní cesty bez úvodního lomítka, takže se rozbaluje
běžným `tar`:

```sh
tar -xzf lafe-arm-20261010-225854.tar.gz -C /cilova/slozka
```
