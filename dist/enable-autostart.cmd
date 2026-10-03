@echo off
setlocal
chcp 65001 >nul
set "TASK_NAME=phone-input-sync"
set "SCRIPT_DIR=%~dp0"

echo.
echo  正在配置「phone-input-sync」开机自启...
echo.

powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $dir=($env:SCRIPT_DIR).TrimEnd('\'); $exe=Join-Path $dir 'phone-input-sync.exe'; if(-not (Test-Path -LiteralPath $exe)){Write-Host ('找不到程序：'+$exe) -ForegroundColor Red; exit 1}; $name=$env:TASK_NAME; $action=New-ScheduledTaskAction -Execute $exe -WorkingDirectory $dir; $trigger=New-ScheduledTaskTrigger -AtLogOn -User ($env:USERDOMAIN+'\'+$env:USERNAME); $settings=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew; $principal=New-ScheduledTaskPrincipal -UserId ($env:USERDOMAIN+'\'+$env:USERNAME) -LogonType Interactive -RunLevel Limited; Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null; $lnk=Join-Path ([Environment]::GetFolderPath('Startup')) 'phone-input-sync.lnk'; if(Test-Path -LiteralPath $lnk){Remove-Item -LiteralPath $lnk -Force}; Write-Host '  已开启开机自启。' -ForegroundColor Green; Write-Host ('  下次登录 Windows 会自动运行：'+$exe)"

if errorlevel 1 (
  echo.
  echo  [失败] 开机自启未能开启，请把上面的错误信息发给作者。
) else (
  echo.
  echo  取消自启：双击同目录的 disable-autostart.cmd
)

echo.
pause
endlocal
