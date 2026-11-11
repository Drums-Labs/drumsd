#!/usr/bin/env python3
"""Generate the Android starter task set (osvm/tasks/android/*.json).

Each task ports an AndroidWorld task (android_world @55ddaf5, task_evals/single/*.py) or is written in its style
("-style" in `source`). Checkers read device state through adb shell exactly as AndroidWorld's is_successful does
(settings, files, content providers, sqlite). Every task has setup (the starting state, re-applied after each reset),
a checker, and an oracle (actions of a known passing run). oracle_kind "shell" means the oracle changes state with
adb shell commands, not through the GUI: it validates the checker, not GUI solvability.
"""
import json, os, sys

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "android")
AW = "android_world@55ddaf5 task_evals/single/"
T = []

def task(id, instruction, source, setup, check, oracle, kind="shell", requires=(), max_steps=10):
    T.append({"id": "android/" + id, "os": "android", "instruction": instruction, "source": source, "setup": setup,
              "check": check, "oracle": oracle, "oracle_kind": kind, "requires": list(requires), "max_steps": max_steps})

def sh(c):
    return {"action_type": "shell", "command": c}

def chk(cmd, **kw):
    d = {"cmd": cmd}
    d.update(kw)
    return d

# --- settings (system.py and AndroidWorld-style)
task("SystemBrightnessMax", "Turn brightness to the max value.", AW + "system.py SystemBrightnessMax",
     ["settings put system screen_brightness_mode 0", "settings put system screen_brightness 102"],
     [chk("settings get system screen_brightness", equals="255")], [sh("settings put system screen_brightness 255")])
task("SystemBrightnessMin", "Turn brightness to the min value.", AW + "system.py SystemBrightnessMin",
     ["settings put system screen_brightness_mode 0", "settings put system screen_brightness 102"],
     [chk("settings get system screen_brightness", equals="1")], [sh("settings put system screen_brightness 1")])
task("AutoRotateOn", "Turn on auto-rotate.", "AndroidWorld-style (system settings)",
     ["settings put system accelerometer_rotation 0"],
     [chk("settings get system accelerometer_rotation", equals="1")], [sh("settings put system accelerometer_rotation 1")])
task("AutoRotateOff", "Turn off auto-rotate.", "AndroidWorld-style (system settings)",
     ["settings put system accelerometer_rotation 1"],
     [chk("settings get system accelerometer_rotation", equals="0")], [sh("settings put system accelerometer_rotation 0")])
task("DarkThemeOn", "Turn on dark theme.", "AndroidWorld-style (system settings)",
     ["cmd uimode night no"], [chk("cmd uimode night", contains="Night mode: yes")], [sh("cmd uimode night yes")])
task("DarkThemeOff", "Turn off dark theme.", "AndroidWorld-style (system settings)",
     ["cmd uimode night yes"], [chk("cmd uimode night", contains="Night mode: no")], [sh("cmd uimode night no")])
task("AirplaneModeOn", "Turn on airplane mode.", "AndroidWorld-style (system settings)",
     ["cmd connectivity airplane-mode disable"], [chk("settings get global airplane_mode_on", equals="1")],
     [sh("cmd connectivity airplane-mode enable")])
task("ScreenTimeout10Min", "Set the screen timeout to 10 minutes.", "AndroidWorld-style (system settings)",
     ["settings put system screen_off_timeout 1800000"], [chk("settings get system screen_off_timeout", equals="600000")],
     [sh("settings put system screen_off_timeout 600000")])
task("FontSizeLargest", "Make the font size as large as possible.", "AndroidWorld-style (system settings)",
     ["settings put system font_scale 1.0"], [chk("settings get system font_scale", min=1.29)],
     [sh("settings put system font_scale 1.3")])
task("LocationOff", "Turn off location.", "AndroidWorld-style (system settings)",
     ["cmd location set-location-enabled true"], [chk("cmd location is-location-enabled", equals="false")],
     [sh("cmd location set-location-enabled false")])

# --- open an app (system.py OpenAppTaskEval): checker = the focused window's package, as get_current_activity
FOCUS = "dumpsys window | grep -E 'mCurrentFocus=' | head -1"
# Contacts is com.android.contacts on AOSP (redroid) and com.google.android.contacts on google_apis (emulator)
# the instruction says to clear pop-ups by granting permissions: the oracle grants the app's pending runtime permissions
LAUNCH = "for p in {pkgs}; do pm path $p >/dev/null 2>&1 && for perm in $(dumpsys package $p | grep -oE 'android.permission.[A-Z_]+: granted=false' | cut -d: -f1); do pm grant $p $perm 2>/dev/null; done; pm path $p >/dev/null 2>&1 && am start -n \"$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER $p | tail -1)\" && break; done"
for name, pkgs in [("Settings", ["com.android.settings"]), ("Contacts", ["com.google.android.contacts", "com.android.contacts"]),
                   ("Markor", ["net.gsantner.markor"]), ("Calendar", ["com.simplemobiletools.calendar.pro"])]:
    task("OpenApp" + name, f"Open the {name} app. Clear any pop-ups that may appear by granting all permissions that are required.",
         AW + "system.py OpenAppTaskEval app_name=" + name.lower(),
         ["input keyevent 3"] + [f"am force-stop {p}" for p in pkgs], [chk(FOCUS, regex="(" + "|".join(p.replace(".", "\\.") for p in pkgs) + ")/")],
         [sh(LAUNCH.format(pkgs=" ".join(pkgs))), {"action_type": "wait", "seconds": 3}], kind="app_launch")

# --- files (files.py)
task("FilesDeleteFile", "Delete the file report_2023.txt from the Download folder.", AW + "files.py FilesDeleteFile",
     ["mkdir -p /sdcard/Download", "echo quarterly > /sdcard/Download/report_2023.txt"],
     [chk("test -e /sdcard/Download/report_2023.txt && echo present || echo absent", equals="absent"),
      chk("test -d /sdcard/Download && echo dir || echo nodir", equals="dir")],
     [sh("rm /sdcard/Download/report_2023.txt")])
task("FilesMoveFile", "Move the file notes.txt from Download to Documents.", AW + "files.py FilesMoveFile",
     ["mkdir -p /sdcard/Download /sdcard/Documents", "echo meeting-notes > /sdcard/Download/notes.txt", "rm -f /sdcard/Documents/notes.txt"],
     [chk("cat /sdcard/Documents/notes.txt", equals="meeting-notes"),
      chk("test -e /sdcard/Download/notes.txt && echo present || echo absent", equals="absent")],
     [sh("mv /sdcard/Download/notes.txt /sdcard/Documents/notes.txt")])

# --- Markor (markor.py): notes are files in /sdcard/Documents/Markor
MK = "/sdcard/Documents/Markor"
task("MarkorCreateNote", "Create a new note in Markor named shopping.md with the text: milk eggs bread.", AW + "markor.py MarkorCreateNote",
     [f"mkdir -p {MK}", f"rm -rf {MK}/*"], [chk(f"cat {MK}/shopping.md", contains="milk eggs bread")],
     [sh(f"echo 'milk eggs bread' > {MK}/shopping.md")], requires=["net.gsantner.markor"])
task("MarkorDeleteNote", "Delete the note old_todo.md in Markor.", AW + "markor.py MarkorDeleteNote",
     [f"mkdir -p {MK}", f"rm -rf {MK}/*", f"echo 'call bank' > {MK}/old_todo.md", f"echo 'keep me' > {MK}/keep.md"],
     [chk(f"test -e {MK}/old_todo.md && echo present || echo absent", equals="absent"), chk(f"cat {MK}/keep.md", equals="keep me")],
     [sh(f"rm {MK}/old_todo.md")], requires=["net.gsantner.markor"])
task("MarkorEditNote", "In Markor, add the line 'pay rent' to the end of the note tasks.md.", AW + "markor.py MarkorEditNote",
     [f"mkdir -p {MK}", f"rm -rf {MK}/*", f"printf 'buy stamps\\n' > {MK}/tasks.md"],
     [chk(f"cat {MK}/tasks.md", regex="buy stamps\\s*\\npay rent\\s*$")],
     [sh(f"printf 'pay rent\\n' >> {MK}/tasks.md")], requires=["net.gsantner.markor"])
task("MarkorCreateFolder", "Create a folder named receipts in Markor.", AW + "markor.py MarkorCreateFolder",
     [f"mkdir -p {MK}", f"rm -rf {MK}/*"], [chk(f"test -d {MK}/receipts && echo dir || echo nodir", equals="dir")],
     [sh(f"mkdir {MK}/receipts")], requires=["net.gsantner.markor"])

# --- contacts (contacts.py ContactsAddContact): checker reads the contacts provider
CQ = "content query --uri content://com.android.contacts/data --projection display_name:data1 --where \"mimetype='vnd.android.cursor.item/phone_v2'\""
ADD = ("content insert --uri content://com.android.contacts/raw_contacts --bind account_name:n: --bind account_type:n:; "
       "id=$(content query --uri content://com.android.contacts/raw_contacts --projection _id | tail -1 | sed 's/.*_id=//'); "
       "content insert --uri content://com.android.contacts/data --bind raw_contact_id:i:$id --bind mimetype:s:vnd.android.cursor.item/name --bind data1:s:'{name}'; "
       "content insert --uri content://com.android.contacts/data --bind raw_contact_id:i:$id --bind mimetype:s:vnd.android.cursor.item/phone_v2 --bind data1:s:{phone} --bind data2:i:2")
task("ContactsAddContact", "Create a new contact for Ada Lovelace. Their number is 555-0142.", AW + "contacts.py ContactsAddContact",
     ["content delete --uri content://com.android.contacts/raw_contacts --where \"_id>0\""],
     [chk(CQ + " | tr -d '-' ", contains="display_name=Ada Lovelace, data1=5550142")],
     [sh(ADD.format(name="Ada Lovelace", phone="5550142"))], requires=["com.android.providers.contacts"])

# --- Markor move (markor.py MarkorMoveNote)
task("MarkorMoveNote", "In Markor, move the note ideas.md into the folder archive.", AW + "markor.py MarkorMoveNote",
     [f"mkdir -p {MK}", f"rm -rf {MK}/*", f"mkdir -p {MK}/archive", f"echo 'solar boat' > {MK}/ideas.md"],
     [chk(f"cat {MK}/archive/ideas.md", equals="solar boat"), chk(f"test -e {MK}/ideas.md && echo present || echo absent", equals="absent")],
     [sh(f"mv {MK}/ideas.md {MK}/archive/ideas.md")], requires=["net.gsantner.markor"])

# --- Simple Calendar Pro (calendar/*.py, sqlite)
CDB = "/data/data/com.simplemobiletools.calendar.pro/databases/events.db"
task("SimpleCalendarAddOneEvent", "In Simple Calendar Pro, create an event titled 'Dentist'.", AW + "calendar/calendar.py SimpleCalendarAddOneEvent",
     ["am force-stop com.simplemobiletools.calendar.pro", f"sqlite3 {CDB} \"delete from events where title='Dentist';\""],
     [chk(f"sqlite3 {CDB} \"select count(*) from events where title='Dentist';\"", equals="1")],
     [sh("am force-stop com.simplemobiletools.calendar.pro; " + f"sqlite3 {CDB} \"insert into events(start_ts,end_ts,title,location,description,reminder_1_minutes,reminder_2_minutes,reminder_3_minutes,reminder_1_type,reminder_2_type,reminder_3_type,repeat_interval,repeat_rule,repeat_limit,repetition_exceptions,attendees,import_id,time_zone,flags,event_type,parent_id,last_updated,source,availability,color,type) values(1700000000,1700003600,'Dentist','','',-1,-1,-1,0,0,0,0,0,0,'[]','[]','','',0,1,0,0,'simple-calendar',0,0,0);\"")],
     requires=["com.simplemobiletools.calendar.pro"])

EVROW = ("(start_ts,end_ts,title,location,description,reminder_1_minutes,reminder_2_minutes,reminder_3_minutes,reminder_1_type,reminder_2_type,"
         "reminder_3_type,repeat_interval,repeat_rule,repeat_limit,repetition_exceptions,attendees,import_id,time_zone,flags,event_type,parent_id,"
         "last_updated,source,availability,color,type) values({ts},{te},'{title}','','',-1,-1,-1,0,0,0,0,0,0,'[]','[]','','',0,1,0,0,'simple-calendar',0,0,0)")
task("SimpleCalendarDeleteEvents", "In Simple Calendar Pro, delete the event titled 'Old meeting'.", AW + "calendar/calendar.py SimpleCalendarDeleteEvents",
     ["am force-stop com.simplemobiletools.calendar.pro",
      f"sqlite3 {CDB} \"delete from events where title in ('Old meeting','Gym'); insert into events" + EVROW.format(ts=1700000000, te=1700003600, title="Old meeting") + "; insert into events" + EVROW.format(ts=1700100000, te=1700103600, title="Gym") + ";\""],
     [chk(f"sqlite3 {CDB} \"select count(*) from events where title='Old meeting';\"", equals="0"),
      chk(f"sqlite3 {CDB} \"select count(*) from events where title='Gym';\"", equals="1")],
     [sh("am force-stop com.simplemobiletools.calendar.pro; " + f"sqlite3 {CDB} \"delete from events where title='Old meeting';\"")],
     requires=["com.simplemobiletools.calendar.pro"])

def main():
    os.makedirs(OUT, exist_ok=True)
    for f in os.listdir(OUT):
        if f.endswith(".json"):
            os.remove(os.path.join(OUT, f))
    contacts = sys.argv[1] if len(sys.argv) > 1 else "com.android.contacts"
    for t in T:
        s = json.dumps(t, indent=1).replace("{CONTACTS}", contacts)
        open(os.path.join(OUT, t["id"].split("/")[1] + ".json"), "w").write(s + "\n")
    print(len(T), "tasks ->", OUT)

if __name__ == "__main__":
    main()
