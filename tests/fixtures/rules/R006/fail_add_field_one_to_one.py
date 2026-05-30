# R006: AddField with OneToOneField - should fail
from django.db import migrations, models


class Migration(migrations.Migration):
    dependencies = []

    operations = [
        migrations.AddField(
            model_name='profile',
            name='user',
            field=models.OneToOneField(on_delete=models.CASCADE, to='auth.user'),
        ),
    ]
