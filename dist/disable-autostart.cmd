@echo off
setlocal
chcp 65001 >nul
set "TASK_NAME=phone-input-sync"

echo.
echo  正在关闭「phone-input-sync」开机自启...
echo.

powershell -NoProfile -ExecutionPolicy Bypass -Command "$name=$env:TASK_NAME; $t=Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue; if($t){Unregister-ScheduledTask -TaskName $name -Confirm:$false; Write-Host '  已关闭开机自启。' -ForegroundColor Green}else{Write-Host '  当前没有设置开机自启，无需关闭。' -ForegroundColor Yellow}; $lnk=Join-Path ([Environment]::GetFolderPath('Startup')) 'phone-input-sync.lnk'; if(Test-Path -LiteralPath $lnk){Remove-Item -LiteralPath $lnk -Force; Write-Host '  已删除「启动」文件夹里的快捷方式。' -ForegroundColor Green}"

if errorlevel 1 (
  echo.
  echo  [失败] 关闭开机自启时出错，请把上面的错误信息发给作者。
)

echo.
pause
endlocal
