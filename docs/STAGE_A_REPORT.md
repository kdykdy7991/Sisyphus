# Stage A 修正报告（Pad 端适配 · Android 工具链打通）

> 日期：2026-09-05
> 结论：**Stage A 全部阻塞项已解除，Android aarch64 release APK 构建成功**。
> 遗留：APK 为**未签名**状态，需在 Stage B 真机安装前配置 keystore（见第 10 节）。

---

## 1. `java -version` 实测输出

```
openjdk version "17.0.20.1" 2026-08-18
OpenJDK Runtime Environment Homebrew (build 17.0.20.1+0)
OpenJDK 64-Bit Server VM Homebrew (build 17.0.20.1+0, mixed mode, sharing)
```

`JAVA_HOME=/opt/homebrew/opt/openjdk@17`。AGP 8.11.0 + Gradle 8.14.3 要求 JDK 17，满足。

## 2. NDK 路径与版本

| 项 | 值 |
| --- | --- |
| NDK 路径 | `/Users/dykong/Library/Android/sdk/ndk/28.2.13676358` |
| NDK 版本 | r28c（`28.2.13676358`） |
| LLVM | **19.0.1** |
| `llvm-ar` | `LLVM version 19.0.1`，darwin-x86_64 prebuilt，Apple Silicon 上可正常执行 |
| 链接器 | LLD 19.0.1 |

```
$ "$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-ar" --version
LLVM version 19.0.1
$ "$NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/clang" --version
Android (13624864, +pgo, -bolt, +lto, -mlgo, based on r530567e) clang version 19.0.1
```

**NDK 26 下载已停止。** 接手时 `ps aux | grep -iE "sdkmanager|ndk|dl.google"` 无任何匹配进程，
`$ANDROID_HOME/ndk` 下只有 `21.1.6352462` 与 `28.2.13676358`，没有 26.1.10909125 残留，
因此无需 kill，也无需清理半成品目录。

## 3. 构建日志中证明使用 NDK 28.2.13676358 的信息

日志文件：`/tmp/tauri_android_build2.log`

**(a) Tauri CLI 明确采用该 NDK：**

```
        Info Using installed NDK: /Users/dykong/Library/Android/sdk/ndk/28.2.13676358
```

**(b) 传给 cargo 的 linker 全部指向 NDK 28：**

```
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = /Users/dykong/Library/Android/sdk/ndk/28.2.13676358/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android24-clang
CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER = .../28.2.13676358/.../armv7a-linux-androideabi24-clang
CARGO_TARGET_I686_LINUX_ANDROID_LINKER     = .../28.2.13676358/.../i686-linux-android24-clang
CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER   = .../28.2.13676358/.../x86_64-linux-android24-clang
NDK_HOME = /Users/dykong/Library/Android/sdk/ndk/28.2.13676358
```

**(c) 产物 `.so` 内嵌的编译器指纹（`llvm-readelf -p .comment`）：**

```
Android (13624864, +pgo, -bolt, +lto, -mlgo, based on r530567e) clang version 19.0.1
Linker: LLD 19.0.1
rustc version 1.98.1 (48a229cea 2026-09-01)
```

clang 19.0.1 即 NDK r28c 自带，且 `cannot find -lunwind` **不再出现**（NDK 21 的硬限制已消失）。

## 4. `src-tauri/.cargo/config.toml` 已删除

- 文件已删除，其所在的空目录 `src-tauri/.cargo/` 也一并移除。
- **没有**做“路径机械替换为 NDK 28”的处理：NDK r28c 的 `llvm-ar` / `aarch64-linux-android24-clang`
  由 Tauri CLI 通过 `CARGO_TARGET_<TARGET>_LINKER` 环境变量自动注入（见 3(b)），不再需要任何 cargo 侧覆写。

残留扫描结果（`grep -rn -E "21\.1\.6352462|26\.1\.10909125|ldndk|--unwindlib|ndk\.dir"`，
排除 `src-tauri/target`、`gen/android/app/build`、`gen/android/.gradle`）：

```
(no output = clean)
```

有效构建配置中已无 NDK 21 / 26 的任何引用。NDK 21 仍安装在磁盘上但完全不被引用，可随时删除。

## 5. 三种 Shell 布局核验结果

核验方式：`npm run build` 产物 + `vite preview` + headless 浏览器实测（读取 `getComputedStyle` 与
`getBoundingClientRect`），截图见本报告第 11 节。

| 视口 | `.bottom-nav` | `.sidebar` | 侧栏宽 | 页面横向溢出 | 导航遮挡内容 |
| --- | --- | --- | --- | --- | --- |
| 桌面 1440×900 | `none` ✅ | `flex` | 238px | `docOverflowX=0`，`mainOverflowX=0` ✅ | 无 ✅ |
| 竖屏 560×840 | `grid` ✅ | `none` ✅ | — | `docOverflowX=0`，`mainOverflowX=0` ✅ | 无：底部导航 `y=780~840`（60px），`.main` `padding-bottom=68px` ≥ 60px ✅ |
| 横屏 840×560 | `none` ✅ | `flex`（紧凑图标） | 76px ✅ | `docOverflowX=0`，`mainOverflowX=0` ✅ | 无 ✅ |

三档全部满足：桌面无底部导航、竖屏显示底部导航、横屏显示紧凑侧栏、无页面级横向滚动、导航不遮挡内容。

本轮针对 Stage A 的 4 处 CSS 修正（`src/styles.css`）：

1. `.bottom-nav` 基础样式改为 `display: none`；仅在 `<=699px` 的两个断点（`540–699`、`<=539`）内
   设 `display: grid`。修正前基础样式是 `display: grid`，桌面/横屏会漏出底部导航。
2. 4 处动态视口声明由 `height:100dvh;height:100vh` 改为 `height:100vh;height:100dvh`
   （`.app-shell` ×2、`.chat` ×2）。原顺序下 `100vh` 后写覆盖 `100dvh`，dvh 等于没生效。
3. 删除脱离媒体查询的全局 `.app-shell{grid-template-rows:auto 1fr}`。它是 Stage A 新加的、
   会让桌面端侧栏行高退化为内容高度（背景不铺满、底部留白），而 `<=699px` 断点本来就有
   `grid-template-rows:1fr auto !important` 覆盖它，删掉不影响 Pad 布局。
4. 构建产物复核：`dist/assets/index-*.css` 中 `.bottom-nav{display:none;...}` 1 处、
   `.bottom-nav{display:grid}` 2 处、`height:100vh;height:100dvh` 4 处、错误顺序 0 处。

## 6. `npm run build` 结果

**通过。**

```
> tsc && vite build
vite v6.4.3 building for production...
✓ 1605 modules transformed.
dist/index.html                   0.48 kB │ gzip:  0.30 kB
dist/assets/index-BfiLtKg5.css   42.47 kB │ gzip:  9.21 kB
dist/assets/index-yUF3xOYx.js   300.81 kB │ gzip: 96.01 kB
✓ built in 1.59s
```

## 7. Android aarch64 构建结果

**成功。**

```
$ npm run tauri android build -- --apk --target aarch64
    Finished `release` profile [optimized] target(s) in 15.24s
        Info symlinking lib .../src-tauri/target/aarch64-linux-android/release/libinterview_kit_lib.so
              in jniLibs dir .../src-tauri/gen/android/app/src/main/jniLibs/arm64-v8a
    ...
    Finished 1 APK at:
        /Users/dykong/Documents/Sisyphus/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk
```

产物校验：

```
$ aapt2 dump badging app-universal-release-unsigned.apk
package: name='com.interviewkit.desktop' versionCode='1000' versionName='0.1.0'
compileSdkVersion='36' compileSdkVersionCodename='16'
minSdkVersion:'24'  targetSdkVersion:'33'
$ unzip -l ... | grep 'lib/'
 27628504  lib/arm64-v8a/libinterview_kit_lib.so      # 仅 arm64-v8a，符合 --target aarch64
```

APK 体积 30,390,095 字节（约 29 MB）。

### 构建过程中修掉的两个额外阻塞

**(a) `compileSdk 33` 与 AndroidX 依赖不兼容 → 升到 36（未降级任何依赖）**

Gradle 明确报错（`/tmp/aarcheck.log`，共 24 条）：

```
Dependency 'androidx.activity:activity-ktx:1.10.1' requires libraries and applications that
depend on it to compile against version 35 or later of the Android APIs.
:app is currently compiled against android-33.
Recommended action: Update this project to use a newer compileSdk of at least 34, for example 36.
```

24 条中 22 条要求 ≥34、2 条（`androidx.activity:activity{,-ktx}:1.10.1`）要求 ≥35；
且 Tauri 自带的 `:tauri-android` 模块自身就是 `compileSdk = 36`。
按 Gradle 建议安装了稳定版 `platforms;android-36`（`AndroidVersion.ApiLevel=36`，`Pkg.Revision=2`），
`app/build.gradle.kts` 改为 `compileSdk = 36`。
**`targetSdk` 保持 33、`minSdk` 保持 24 未动**，避免 API 35 强制 edge-to-edge 在 Stage B 之前引入
不可控的布局行为变化。所有 AndroidX 依赖版本保持原样，未降级。

**(b) 生成的 `BuildTask.kt` 调用 tauri CLI 失败**

`tauri android init` 生成的
`src-tauri/gen/android/buildSrc/src/main/java/com/interviewkit/desktop/kotlin/BuildTask.kt`
里是 `node` + 参数 `["tauri", "android", "android-studio-script"]`，工作目录为 `src-tauri`；
node 会把第一个参数当脚本路径解析，于是：

```
Error: Cannot find module '/Users/dykong/Documents/Sisyphus/src-tauri/tauri'
> Execution failed for task ':app:rustBuildArm64Release'.
```

已改为指向 npm 安装的 CLI 入口（相对路径，不含本机绝对路径）：

```kotlin
val args = listOf("../node_modules/@tauri-apps/cli/tauri.js", "android", "android-studio-script");
```

## 8. APK 绝对路径与签名类型

| 项 | 值 |
| --- | --- |
| 绝对路径 | `/Users/dykong/Documents/Sisyphus/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk` |
| 构建类型 | release（`Finished \`release\` profile [optimized]\`），Gradle 变体 `universalRelease`，ABI 实际只有 `arm64-v8a` |
| **签名类型** | **未签名（unsigned）** |

未签名的证据：

```
$ unzip -l ... | grep -E 'META-INF/.*\.(RSA|DSA|EC|SF|MF)$'
（无匹配，无 v1 签名）
$ apksigner verify --print-certs ...
DOES NOT VERIFY
ERROR: Missing META-INF/MANIFEST.MF
```

原因：`src-tauri/tauri.android.conf.json` 的 `bundle.android` 只配了 `minSdkVersion`，
没有 `keystore` / `keystorePassword` / `keyPassword`，Tauri 不会自动签名 release 包。
**该 APK 无法直接安装到真机**，Stage B 真机验收前需二选一：

- 在 `src-tauri/tauri.android.conf.json` 增加签名配置：
  ```json
  "bundle": {
    "android": {
      "minSdkVersion": 24,
      "keystore": "/绝对路径/interview-kit.keystore",
      "keystorePassword": "……",
      "keyPassword": "……",
      "keyAlias": "interview-kit"
    }
  }
  ```
  （口令建议走环境变量，不要把明文口令入库。）
- 或先跑 `npm run tauri android build -- --apk --target aarch64 --debug`，Tauri 会用 Android
  debug keystore 自动签名，可装机自测，但不能作为最终分发包。

## 9. 不存在“第一处有效错误”

第 7 节构建已成功，无需提供失败上下文。第 7 节 (a)(b) 记录的是**本轮修掉的两个阻塞项**及其第一处有效错误。

## 10. NDK 版本锁定的持久化方式

- 锁定位置：`src-tauri/gen/android/app/build.gradle.kts` 的 `android { ndkVersion = "28.2.13676358" }`。
  按要求**未使用 `ndkPath`**，因此不含任何本机绝对路径。
- 工程目录 `src-tauri/gen/android/` 目前是未跟踪状态（`git status` 显示 `??`），
  其中 `tauri.settings.gradle`（含 cargo registry 绝对路径）与 `app/build`、`.gradle`、
  `local.properties` 已被 Tauri 生成的 `.gitignore` 排除。

**如何保证重新生成后仍锁定 r28c：**

1. `tauri android init` 在 Android 工程已存在时不会覆盖全部文件，但**会重写**
   `app/build.gradle.kts` 与 `buildSrc/**/BuildTask.kt`。因此这两处人工修改属于
   “可重放补丁”，不是一次性修改。
2. 把 `src-tauri/gen/android/` 提交进版本库（排除项已由 Tauri 自带的 `.gitignore` 处理好）。
   提交后，任何人重新执行 `tauri android init` 都会让 `git status` 立刻显示
   `ndkVersion` 被抹掉、`BuildTask.kt` 回到 `listOf("tauri", ...)`，可在 review 时拦截。
3. 重新生成后需要重放的两处改动（均为纯相对路径/版本号，可安全重复执行）：
   - `app/build.gradle.kts` → `android {}` 内加 `ndkVersion = "28.2.13676358"`、`compileSdk = 36`
   - `buildSrc/src/main/java/com/interviewkit/desktop/kotlin/BuildTask.kt` →
     `listOf("tauri", ...)` 改为 `listOf("../node_modules/@tauri-apps/cli/tauri.js", ...)`
4. 环境侧另需保证 `ANDROID_HOME` 下有 `ndk/28.2.13676358`；当前机器上 NDK 21 仍在与 28 并存，
   因为已显式锁定 `ndkVersion`，Gradle 不会误选 21。

## 11. 布局核验截图

| 视口 | 截图 |
| --- | --- |
| 桌面 1440×900 | `docs/screenshots/stage-a/shell_1440x900.png` |
| 竖屏 560×840 | `docs/screenshots/stage-a/shell_560x840.png` |
| 横屏 840×560 | `docs/screenshots/stage-a/shell_840x560.png` |

## 12. 本轮改动清单

| 文件 | 改动 |
| --- | --- |
| `src/styles.css` | `.bottom-nav` 默认 `display:none`，`<=699px` 两断点内 `display:grid`；4 处 `100vh/100dvh` 顺序修正；删除全局 `.app-shell{grid-template-rows:auto 1fr}` |
| `src-tauri/.cargo/config.toml` | 删除（含空目录） |
| `src-tauri/gen/android/app/build.gradle.kts` | 新增 `ndkVersion = "28.2.13676358"`；`compileSdk` 33 → 36 |
| `src-tauri/gen/android/buildSrc/.../BuildTask.kt` | CLI 调用参数改为 `../node_modules/@tauri-apps/cli/tauri.js` |
| `docs/STAGE_A_REPORT.md` | 本报告 |
| Android SDK | 新增 `platforms;android-36`（Tauri 生成的 `.gitignore` 之外，属本机环境） |

未改动：`index.html`、`src/components/Shell.tsx`（本次无需改动）、
`src-tauri/tauri.android.conf.json`（签名配置待 Stage B 决定 keystore 方案后再改）。
