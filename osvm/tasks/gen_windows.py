#!/usr/bin/env python3
"""Generate the Windows starter task set (osvm/tasks/windows/*.json), WindowsAgentArena-style.

Domains follow WindowsAgentArena's (file_explorer, notepad, settings, vs_code, libreoffice, msedge/paint/clock as
'windows' misc). Checkers are PowerShell commands run through the in-guest server, judged like WAA's getters +
metrics (file exists/content, registry value). Setups put the starting state AND remove the task's target
outputs, so an arm whose reset does not restore a clean image is not failed by a previous episode's output
(state left behind elsewhere is measured separately by the state-hash audit). oracle_kind "shell": the oracle
changes state with PowerShell, which validates the checker, not GUI solvability.
"""
import json, os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "windows")
T = []
D = "([Environment]::GetFolderPath('Desktop'))"
DOC = "([Environment]::GetFolderPath('MyDocuments'))"
PIC = "([Environment]::GetFolderPath('MyPictures'))"
ADV = "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced"
THM = "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"

def task(id, instruction, domain, setup, check, oracle):
    T.append({"id": "windows/" + id, "os": "windows", "instruction": instruction,
              "source": f"WindowsAgentArena-style, domain {domain} (WindowsAgentArena@eb220c4 evaluation_examples_windows)",
              "setup": setup, "check": check, "oracle": [{"type": "powershell", "command": c} for c in oracle],
              "oracle_kind": "shell", "requires": [], "max_steps": 15})

def chk(cmd, **kw):
    d = {"cmd": cmd}; d.update(kw); return d

def p(base, *parts):
    return "(Join-Path " + base + " '" + "\\".join(parts) + "')"

YESNO = "if (Test-Path {0}) {{ 'yes' }} else {{ 'no' }}"

# file_explorer
task("FileExplorerCreateFolder", "Create a folder named Reports on the Desktop.", "file_explorer",
     [f"Remove-Item -Recurse -Force {p(D,'Reports')} -ErrorAction SilentlyContinue; exit 0"],
     [chk(f"if (Test-Path -PathType Container {p(D,'Reports')}) {{ 'yes' }} else {{ 'no' }}", equals="yes")],
     [f"New-Item -ItemType Directory {p(D,'Reports')} | Out-Null"])
task("FileExplorerRenameFile", "Rename draft.txt on the Desktop to final.txt.", "file_explorer",
     [f"Remove-Item -Force {p(D,'final.txt')} -ErrorAction SilentlyContinue; exit 0", f"Set-Content {p(D,'draft.txt')} 'v1'"],
     [chk(YESNO.format(p(D,'final.txt')), equals="yes"), chk(YESNO.format(p(D,'draft.txt')), equals="no")],
     [f"Rename-Item {p(D,'draft.txt')} final.txt"])
task("FileExplorerDeleteFile", "Delete the file old.log from the Desktop.", "file_explorer",
     [f"Set-Content {p(D,'old.log')} 'x'"], [chk(YESNO.format(p(D,'old.log')), equals="no")],
     [f"Remove-Item {p(D,'old.log')}"])
task("FileExplorerMoveFile", "Move photo_list.txt from the Desktop to Documents.", "file_explorer",
     [f"Remove-Item -Force {p(DOC,'photo_list.txt')} -ErrorAction SilentlyContinue; exit 0", f"Set-Content {p(D,'photo_list.txt')} 'a.jpg'"],
     [chk(f"Get-Content {p(DOC,'photo_list.txt')}", equals="a.jpg"), chk(YESNO.format(p(D,'photo_list.txt')), equals="no")],
     [f"Move-Item {p(D,'photo_list.txt')} {p(DOC,'photo_list.txt')}"])
task("FileExplorerCopyFile", "Copy budget.csv from Documents to the Desktop.", "file_explorer",
     [f"Remove-Item -Force {p(D,'budget.csv')} -ErrorAction SilentlyContinue; exit 0", f"Set-Content {p(DOC,'budget.csv')} 'item,cost'"],
     [chk(f"Get-Content {p(D,'budget.csv')}", equals="item,cost"), chk(f"Get-Content {p(DOC,'budget.csv')}", equals="item,cost")],
     [f"Copy-Item {p(DOC,'budget.csv')} {p(D,'budget.csv')}"])
task("FileExplorerZipFolder", "Compress the folder 'project' on the Desktop into project.zip on the Desktop.", "file_explorer",
     [f"Remove-Item -Force {p(D,'project.zip')} -ErrorAction SilentlyContinue; exit 0", f"New-Item -Force -ItemType Directory {p(D,'project')} | Out-Null",
      f"Set-Content {p(D,'project','a.txt')} 'a'", f"Set-Content {p(D,'project','b.txt')} 'b'"],
     [chk(f"Add-Type -A System.IO.Compression.FileSystem; ([IO.Compression.ZipFile]::OpenRead({p(D,'project.zip')}).Entries | Where-Object {{ $_.Name -ne '' }}).Count", equals="2")],
     [f"Compress-Archive -Path {p(D,'project')} -DestinationPath {p(D,'project.zip')}"])
task("FileExplorerShowExtensions", "Make File Explorer show file name extensions.", "file_explorer",
     [f"Set-ItemProperty '{ADV}' HideFileExt 1"], [chk(f"(Get-ItemProperty '{ADV}').HideFileExt", equals="0")],
     [f"Set-ItemProperty '{ADV}' HideFileExt 0"])
task("FileExplorerShowHidden", "Make File Explorer show hidden files.", "file_explorer",
     [f"Set-ItemProperty '{ADV}' Hidden 2"], [chk(f"(Get-ItemProperty '{ADV}').Hidden", equals="1")],
     [f"Set-ItemProperty '{ADV}' Hidden 1"])
# notepad
task("NotepadCreateNote", "Create todo.txt on the Desktop containing the text 'buy milk'.", "notepad",
     [f"Remove-Item -Force {p(D,'todo.txt')} -ErrorAction SilentlyContinue; exit 0"], [chk(f"Get-Content -Raw {p(D,'todo.txt')}", contains="buy milk")],
     [f"Set-Content {p(D,'todo.txt')} 'buy milk'"])
task("NotepadAppendLine", "Open notes.txt on the Desktop and add a last line 'line two'.", "notepad",
     [f"Set-Content {p(D,'notes.txt')} 'line one'"], [chk(f"(Get-Content {p(D,'notes.txt')}) -join '|'", equals="line one|line two")],
     [f"Add-Content {p(D,'notes.txt')} 'line two'"])
task("NotepadReplaceWord", "In story.txt on the Desktop, replace the word 'cat' with 'dog'.", "notepad",
     [f"Set-Content {p(D,'story.txt')} 'the cat sat'"], [chk(f"Get-Content {p(D,'story.txt')}", equals="the dog sat")],
     [f"Set-Content {p(D,'story.txt')} 'the dog sat'"])
# settings
task("SettingsDarkModeApps", "Switch apps to dark mode.", "settings",
     [f"Set-ItemProperty '{THM}' AppsUseLightTheme 1"], [chk(f"(Get-ItemProperty '{THM}').AppsUseLightTheme", equals="0")],
     [f"Set-ItemProperty '{THM}' AppsUseLightTheme 0"])
task("SettingsDarkModeSystem", "Switch Windows (taskbar and Start) to dark mode.", "settings",
     [f"Set-ItemProperty '{THM}' SystemUsesLightTheme 1"], [chk(f"(Get-ItemProperty '{THM}').SystemUsesLightTheme", equals="0")],
     [f"Set-ItemProperty '{THM}' SystemUsesLightTheme 0"])
task("SettingsTaskbarLeft", "Align the taskbar icons to the left.", "settings",
     [f"Set-ItemProperty '{ADV}' TaskbarAl 1"], [chk(f"(Get-ItemProperty '{ADV}').TaskbarAl", equals="0")],
     [f"Set-ItemProperty '{ADV}' TaskbarAl 0"])
task("SettingsTimeZone", "Set the time zone to Pacific Time (US & Canada).", "settings",
     ["Set-TimeZone -Id 'UTC'"], [chk("(Get-TimeZone).Id", equals="Pacific Standard Time")],
     ["Set-TimeZone -Id 'Pacific Standard Time'"])
task("SettingsScreenSaverTimeout", "Set the screen saver wait time to 10 minutes.", "settings",
     ["Set-ItemProperty 'HKCU:\\Control Panel\\Desktop' ScreenSaveTimeOut '900'"],
     [chk("(Get-ItemProperty 'HKCU:\\Control Panel\\Desktop').ScreenSaveTimeOut", equals="600")],
     ["Set-ItemProperty 'HKCU:\\Control Panel\\Desktop' ScreenSaveTimeOut '600'"])
task("SettingsUserEnvVar", "Add a user environment variable PROJECT_HOME with value C:\\work.", "settings",
     ["Remove-ItemProperty 'HKCU:\\Environment' PROJECT_HOME -ErrorAction SilentlyContinue; exit 0"],
     [chk("(Get-ItemProperty 'HKCU:\\Environment').PROJECT_HOME", equals="C:\\work")],
     ["Set-ItemProperty 'HKCU:\\Environment' PROJECT_HOME 'C:\\work'"])
task("SettingsMouseSwap", "Swap the primary and secondary mouse buttons.", "settings",
     ["Set-ItemProperty 'HKCU:\\Control Panel\\Mouse' SwapMouseButtons '0'"],
     [chk("(Get-ItemProperty 'HKCU:\\Control Panel\\Mouse').SwapMouseButtons", equals="1")],
     ["Set-ItemProperty 'HKCU:\\Control Panel\\Mouse' SwapMouseButtons '1'"])
# vs_code
VSC = "(Join-Path $env:APPDATA 'Code\\User\\settings.json')"
task("VSCodeFontSize", "In VS Code, set the editor font size to 18.", "vs_code",
     [f"New-Item -Force -ItemType Directory (Split-Path {VSC}) | Out-Null", f"Set-Content {VSC} '{{}}'"],
     [chk(f"(Get-Content -Raw {VSC} | ConvertFrom-Json).'editor.fontSize'", equals="18")],
     [f"Set-Content {VSC} '{{\"editor.fontSize\": 18}}'"])
task("VSCodeCreateFile", "Create hello.py on the Desktop that prints hello.", "vs_code",
     [f"Remove-Item -Force {p(D,'hello.py')} -ErrorAction SilentlyContinue; exit 0"], [chk(f"Get-Content -Raw {p(D,'hello.py')}", regex="print\\(['\\\"]hello['\\\"]\\)")],
     [f"Set-Content {p(D,'hello.py')} 'print(\"hello\")'"])
# libreoffice (calc)
task("CalcSaveCsv", "Create sales.csv in Documents whose first row is item,qty.", "libreoffice_calc",
     [f"Remove-Item -Force {p(DOC,'sales.csv')} -ErrorAction SilentlyContinue; exit 0"], [chk(f"@(Get-Content {p(DOC,'sales.csv')})[0]", equals="item,qty")],
     [f"Set-Content {p(DOC,'sales.csv')} 'item,qty'"])
# paint / windows misc
task("PaintSaveImage", "Save a new image named canvas.png in Pictures.", "microsoft_paint",
     [f"Remove-Item -Force {p(PIC,'canvas.png')} -ErrorAction SilentlyContinue; exit 0"],
     [chk(f"$b=[IO.File]::ReadAllBytes({p(PIC,'canvas.png')}); '{{0:X2}}{{1:X2}}{{2:X2}}{{3:X2}}' -f $b[0],$b[1],$b[2],$b[3]", equals="89504E47")],
     [f"Add-Type -A System.Drawing; $bm=New-Object Drawing.Bitmap 64,64; $bm.Save({p(PIC,'canvas.png')}, [Drawing.Imaging.ImageFormat]::Png)"])
task("CreateShortcut", "Create a Desktop shortcut to Notepad named Notepad.", "windows",
     [f"Remove-Item -Force {p(D,'Notepad.lnk')} -ErrorAction SilentlyContinue; exit 0"],
     [chk(f"(New-Object -ComObject WScript.Shell).CreateShortcut({p(D,'Notepad.lnk')}).TargetPath", regex="(?i)notepad\\.exe$")],
     [f"$s=(New-Object -ComObject WScript.Shell).CreateShortcut({p(D,'Notepad.lnk')}); $s.TargetPath='C:\\Windows\\System32\\notepad.exe'; $s.Save()"])
task("RecycleBinEmpty", "Empty the Recycle Bin.", "file_explorer",
     [f"Set-Content {p(D,'trash_me.txt')} 'x'", "Add-Type -A Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile((Join-Path ([Environment]::GetFolderPath('Desktop')) 'trash_me.txt'),'OnlyErrorDialogs','SendToRecycleBin')"],
     [chk("(New-Object -ComObject Shell.Application).NameSpace(10).Items().Count", equals="0")],
     ["Clear-RecycleBin -Force -ErrorAction SilentlyContinue; exit 0"])

if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    for f in os.listdir(OUT):
        if f.endswith(".json"):
            os.remove(os.path.join(OUT, f))
    for t in T:
        json.dump(t, open(os.path.join(OUT, t["id"].split("/")[1] + ".json"), "w"), indent=1)
    print(len(T), "tasks ->", OUT)
