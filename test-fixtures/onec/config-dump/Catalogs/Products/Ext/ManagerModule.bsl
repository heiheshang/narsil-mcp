Procedure BeforeWrite(Cancel, WriteMode)
    If Not ValueFilled(Code) Then
        Code = Utilities.FormatItemCode("0001");
    EndIf;
EndProcedure
