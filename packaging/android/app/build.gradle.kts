plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

val generatedLauncherResDir = layout.buildDirectory.dir("generated/res/launcherIcons")

val stageLauncherIcons by tasks.registering {
    val iconRoot = rootProject.projectDir.resolve("../../assets/app-icon/hicolor")
    inputs.dir(iconRoot)
    outputs.dir(generatedLauncherResDir)
    doLast {
        val densityToSize = mapOf(
            "mipmap-mdpi" to "48x48",
            "mipmap-hdpi" to "64x64",
            "mipmap-xhdpi" to "128x128",
            "mipmap-xxhdpi" to "256x256",
            "mipmap-xxxhdpi" to "512x512",
        )
        val outRoot = generatedLauncherResDir.get().asFile
        densityToSize.forEach { (densityDir, sizeDir) ->
            val srcFile = iconRoot.resolve("$sizeDir/apps/ai.storyteller.filmcraft.png")
            check(srcFile.isFile) { "Missing FilmCraft app icon at $srcFile" }
            val dstFile = outRoot.resolve("$densityDir/ic_launcher.png")
            dstFile.parentFile.mkdirs()
            srcFile.copyTo(dstFile, overwrite = true)
        }
    }
}

tasks.named("preBuild") {
    dependsOn(stageLauncherIcons)
}

android {
    namespace = "ai.storyteller.filmcraft"
    compileSdk = 34
    ndkVersion = "26.1.10909125"

    defaultConfig {
        applicationId = "ai.storyteller.filmcraft"
        minSdk = 26
        targetSdk = 34
        versionCode = 1400
        versionName = "0.14.0"

        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
        debug {
            isJniDebuggable = true
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
            res.srcDirs("src/main/res", generatedLauncherResDir)
        }
    }
}
