# Shrink unused code/resources while preserving readable names and JNI/WebView entrypoints.
-dontobfuscate

-keep class com.animepahe.app.** { *; }

-keepclassmembers class com.animepahe.app.NativeEngine {
    native <methods>;
}

-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}
