import java.util.Properties
import java.io.FileInputStream

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.animepahe.app"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.animepahe.app"
        minSdk = 24
        targetSdk = 35
        versionCode = 17
        versionName = "2.0.5"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        ndk {
            abiFilters.addAll(listOf("x86_64", "arm64-v8a", "armeabi-v7a"))
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
    }

    val keystorePropertiesFile = file("keystore.properties")
    val keystoreProperties = Properties()
    if (keystorePropertiesFile.exists()) {
        FileInputStream(keystorePropertiesFile).use { stream ->
            keystoreProperties.load(stream)
        }
    } else {
        logger.warn("android/app/keystore.properties not found. Signing credentials must be provided in that file; see README.")
    }

    signingConfigs {
        create("release") {
            fun signingProp(name: String): String =
                checkNotNull(keystoreProperties.getProperty(name)) {
                    "android/app/keystore.properties is missing '$name'. Signing credentials must come " +
                        "from keystore.properties — hardcoded fallback passwords are not allowed."
                }

            storeFile = file(signingProp("storeFile"))
            storePassword = signingProp("storePassword")
            keyAlias = signingProp("keyAlias")
            keyPassword = signingProp("keyPassword")
            enableV1Signing = true
            enableV2Signing = true
            enableV3Signing = true
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            signingConfig = signingConfigs.getByName("release")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
        debug {
            signingConfig = signingConfigs.getByName("release")
            isDebuggable = true
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.webkit:webkit:1.12.1")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.activity:activity-ktx:1.9.3")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
}
