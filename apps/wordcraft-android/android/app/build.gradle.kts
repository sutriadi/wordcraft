plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.storytold.wordcraft"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.storytold.wordcraft"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.7.0"
        // Only the ABIs `build-android.sh` produces.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    buildTypes {
        release { isMinifyEnabled = false }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    // libwordcraft_android.so is copied here by build-android.sh (cargo-ndk).
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")
}
