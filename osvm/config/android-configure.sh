# configure() of osvm/hosts/android-golden.sh as one adb shell command per line (for incumbent arms)
settings put global window_animation_scale 0.0; settings put global transition_animation_scale 0.0; settings put global animator_duration_scale 0.0
settings put global hidden_api_policy_pre_p_apps 1; settings put global hidden_api_policy_p_apps 1; settings put global hidden_api_policy 1
svc power stayon true; settings put system screen_off_timeout 1800000; settings put system screen_brightness 102; settings put system screen_brightness_mode 0; settings put system accelerometer_rotation 0
mkdir -p /sdcard/Documents/Markor /sdcard/Download /sdcard/Documents
am start -n "$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER net.gsantner.markor | tail -1)"
am force-stop net.gsantner.markor
am start -n "$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.arduia.expense | tail -1)"
am force-stop com.arduia.expense
am start -n "$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.simplemobiletools.calendar.pro | tail -1)"
am force-stop com.simplemobiletools.calendar.pro
am start -n "$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.android.contacts | tail -1)"
am force-stop com.android.contacts
am start -n "$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.android.settings | tail -1)"
am force-stop com.android.settings
input keyevent 82; input keyevent 3
