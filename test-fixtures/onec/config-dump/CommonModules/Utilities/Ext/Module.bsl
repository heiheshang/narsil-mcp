Function FormatItemCode(Code)
    Return "ITM-" + Code;
EndFunction

Procedure NotifyUser(Message) Export
    Message(Message);
EndProcedure
