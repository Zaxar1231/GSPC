# Triplet vcpkg для сборки ANGLE в workflow `angle`: x64, DLL, только release.
#
# ANGLE_ENABLE_SHARE_CONTEXT_LOCK и ANGLE_ENABLE_CONTEXT_MUTEX включены в родной сборке ANGLE (GN)
# по умолчанию, но CMake-сборка порта vcpkg их не задаёт. Без них контексты с общими объектами
# (share group) не синхронизируются, и вызывающий сам обязан не вызывать GL из разных потоков
# одновременно. gfxstream рисует из нескольких потоков с общими контекстами — без блокировки
# ANGLE падал внутри glDrawArrays через полминуты после старта Android.
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE dynamic)
set(VCPKG_LIBRARY_LINKAGE dynamic)
set(VCPKG_BUILD_TYPE release)
set(VCPKG_C_FLAGS "/DANGLE_ENABLE_SHARE_CONTEXT_LOCK=1 /DANGLE_ENABLE_CONTEXT_MUTEX=1")
set(VCPKG_CXX_FLAGS "/DANGLE_ENABLE_SHARE_CONTEXT_LOCK=1 /DANGLE_ENABLE_CONTEXT_MUTEX=1")
